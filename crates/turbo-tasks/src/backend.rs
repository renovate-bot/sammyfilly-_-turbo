use std::{
    any::Any,
    borrow::Cow,
    fmt,
    fmt::{Debug, Display},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};

pub use crate::id::BackendJobId;
use crate::{
    event::EventListener, manager::TurboTasksBackendApi, primitives::RawVcSetVc, raw_vc::CellId,
    registry, task_input::SharedReference, FunctionId, RawVc, ReadRef, TaskId, TaskIdProvider,
    TaskInput, TraitRef, TraitTypeId, ValueTraitVc,
};

pub enum TaskType {
    /// Tasks that only exist for a certain operation and
    /// won't persist between sessions
    Transient(TransientTaskType),

    /// Tasks that can persist between sessions and potentially
    /// shared globally
    Persistent(PersistentTaskType),
}

type TransientTaskRoot =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<RawVc>> + Send>> + Send + Sync>;

pub enum TransientTaskType {
    /// A root task that will track dependencies and re-execute when
    /// dependencies change. Task will eventually settle to the correct
    /// execution.
    /// Always active. Automatically scheduled.
    Root(TransientTaskRoot),

    // TODO implement these strongly consistency
    /// A single root task execution. It won't track dependencies.
    /// Task will definitely include all invalidations that happened before the
    /// start of the task. It may or may not include invalidations that
    /// happened after that. It may see these invalidations partially
    /// applied.
    /// Active until done. Automatically scheduled.
    Once(Pin<Box<dyn Future<Output = Result<RawVc>> + Send + 'static>>),
}

impl Debug for TransientTaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Root(_) => f.debug_tuple("Root").finish(),
            Self::Once(_) => f.debug_tuple("Once").finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PersistentTaskType {
    /// A normal task execution a native (rust) function
    Native(FunctionId, Vec<TaskInput>),

    /// A resolve task, which resolves arguments and calls the function with
    /// resolve arguments. The inner function call will do a cache lookup.
    ResolveNative(FunctionId, Vec<TaskInput>),

    /// A trait method resolve task. It resolves the first (`self`) argument and
    /// looks up the trait method on that value. Then it calls that method.
    /// The method call will do a cache lookup and might resolve arguments
    /// before.
    ResolveTrait(TraitTypeId, Cow<'static, str>, Vec<TaskInput>),
}

impl Display for PersistentTaskType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native(fid, _) | Self::ResolveNative(fid, _) => {
                Display::fmt(&registry::get_function(*fid).name, f)
            }
            Self::ResolveTrait(tid, n, _) => {
                write!(f, "{}::{n}", registry::get_trait(*tid).name)
            }
        }
    }
}

impl PersistentTaskType {
    pub fn shrink_to_fit(&mut self) {
        match self {
            Self::Native(_, inputs) => inputs.shrink_to_fit(),
            Self::ResolveNative(_, inputs) => inputs.shrink_to_fit(),
            Self::ResolveTrait(_, _, inputs) => inputs.shrink_to_fit(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            PersistentTaskType::Native(_, v)
            | PersistentTaskType::ResolveNative(_, v)
            | PersistentTaskType::ResolveTrait(_, _, v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            PersistentTaskType::Native(_, v)
            | PersistentTaskType::ResolveNative(_, v)
            | PersistentTaskType::ResolveTrait(_, _, v) => v.is_empty(),
        }
    }

    pub fn partial(&self, len: usize) -> Self {
        match self {
            PersistentTaskType::Native(f, v) => PersistentTaskType::Native(*f, v[..len].to_vec()),
            PersistentTaskType::ResolveNative(f, v) => {
                PersistentTaskType::ResolveNative(*f, v[..len].to_vec())
            }
            PersistentTaskType::ResolveTrait(f, n, v) => {
                PersistentTaskType::ResolveTrait(*f, n.clone(), v[..len].to_vec())
            }
        }
    }
}

pub struct TaskExecutionSpec {
    pub future: Pin<Box<dyn Future<Output = Result<RawVc>> + Send>>,
}

// TODO technically CellContent is already indexed by the ValueTypeId, so we
// don't need to store it here
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellContent(pub Option<SharedReference>);

impl Display for CellContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            None => write!(f, "empty"),
            Some(content) => Display::fmt(content, f),
        }
    }
}

impl CellContent {
    pub fn cast<T: Any + Send + Sync>(self) -> Result<ReadRef<T>> {
        let data = self.0.ok_or_else(|| anyhow!("Cell is empty"))?;
        let data = data
            .downcast()
            .ok_or_else(|| anyhow!("Unexpected type in cell"))?;
        Ok(ReadRef::new(data))
    }

    /// # Safety
    ///
    /// T and U must be binary identical (#[repr(transparent)])
    pub unsafe fn cast_transparent<T: Any + Send + Sync, U>(self) -> Result<ReadRef<T, U>> {
        let data = self.0.ok_or_else(|| anyhow!("Cell is empty"))?;
        let data = data
            .downcast()
            .ok_or_else(|| anyhow!("Unexpected type in cell"))?;
        Ok(unsafe { ReadRef::new_transparent(data) })
    }

    /// # Safety
    ///
    /// The caller must ensure that the CellContent contains a vc that
    /// implements T.
    pub fn cast_trait<T>(self) -> Result<TraitRef<T>>
    where
        T: ValueTraitVc,
    {
        let shared_reference = self.0.ok_or_else(|| anyhow!("Cell is empty"))?;
        if shared_reference.0.is_none() {
            bail!("Cell content is untyped");
        }
        Ok(
            // Safety: We just checked that the content is typed.
            TraitRef::new(shared_reference),
        )
    }

    pub fn try_cast<T: Any + Send + Sync>(self) -> Option<ReadRef<T>> {
        self.0
            .and_then(|data| data.downcast().map(|data| ReadRef::new(data)))
    }
}

pub trait Backend: Sync + Send {
    #[allow(unused_variables)]
    fn initialize(&mut self, task_id_provider: &dyn TaskIdProvider) {}

    #[allow(unused_variables)]
    fn startup(&self, turbo_tasks: &dyn TurboTasksBackendApi<Self>) {}

    #[allow(unused_variables)]
    fn stop(&self, turbo_tasks: &dyn TurboTasksBackendApi<Self>) {}

    #[allow(unused_variables)]
    fn idle_start(&self, turbo_tasks: &dyn TurboTasksBackendApi<Self>) {}

    fn invalidate_task(&self, task: TaskId, turbo_tasks: &dyn TurboTasksBackendApi<Self>);

    fn invalidate_tasks(&self, tasks: Vec<TaskId>, turbo_tasks: &dyn TurboTasksBackendApi<Self>);

    fn get_task_description(&self, task: TaskId) -> String;

    type ExecutionScopeFuture<T: Future<Output = Result<()>> + Send + 'static>: Future<Output = Result<()>>
        + Send
        + 'static;

    fn execution_scope<T: Future<Output = Result<()>> + Send + 'static>(
        &self,
        task: TaskId,
        future: T,
    ) -> Self::ExecutionScopeFuture<T>;

    fn try_start_task_execution(
        &self,
        task: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> Option<TaskExecutionSpec>;

    fn task_execution_result(
        &self,
        task: TaskId,
        result: Result<Result<RawVc>, Option<Cow<'static, str>>>,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    );

    fn task_execution_completed(
        &self,
        task: TaskId,
        duration: Duration,
        instant: Instant,
        stateful: bool,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> bool;

    fn run_backend_job<'a>(
        &'a self,
        id: BackendJobId,
        turbo_tasks: &'a dyn TurboTasksBackendApi<Self>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    fn try_read_task_output(
        &self,
        task: TaskId,
        reader: TaskId,
        strongly_consistent: bool,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> Result<Result<RawVc, EventListener>>;

    /// INVALIDATION: Be careful with this, it will not track dependencies, so
    /// using it could break cache invalidation.
    fn try_read_task_output_untracked(
        &self,
        task: TaskId,
        strongly_consistent: bool,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> Result<Result<RawVc, EventListener>>;

    fn try_read_task_cell(
        &self,
        task: TaskId,
        index: CellId,
        reader: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> Result<Result<CellContent, EventListener>>;

    /// INVALIDATION: Be careful with this, it will not track dependencies, so
    /// using it could break cache invalidation.
    fn try_read_task_cell_untracked(
        &self,
        task: TaskId,
        index: CellId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> Result<Result<CellContent, EventListener>>;

    /// INVALIDATION: Be careful with this, it will not track dependencies, so
    /// using it could break cache invalidation.
    fn try_read_own_task_cell_untracked(
        &self,
        current_task: TaskId,
        index: CellId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> Result<CellContent> {
        match self.try_read_task_cell_untracked(current_task, index, turbo_tasks)? {
            Ok(content) => Ok(content),
            Err(_) => Ok(CellContent(None)),
        }
    }

    fn read_task_collectibles(
        &self,
        task: TaskId,
        trait_id: TraitTypeId,
        reader: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> RawVcSetVc;

    fn emit_collectible(
        &self,
        trait_type: TraitTypeId,
        collectible: RawVc,
        task: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    );

    fn unemit_collectible(
        &self,
        trait_type: TraitTypeId,
        collectible: RawVc,
        task: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    );

    fn update_task_cell(
        &self,
        task: TaskId,
        index: CellId,
        content: CellContent,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    );

    fn get_or_create_persistent_task(
        &self,
        task_type: PersistentTaskType,
        parent_task: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> TaskId;

    fn connect_task(
        &self,
        task: TaskId,
        parent_task: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    );

    fn mark_own_task_as_finished(
        &self,
        _task: TaskId,
        _turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) {
        // Do nothing by default
    }

    fn create_transient_task(
        &self,
        task_type: TransientTaskType,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> TaskId;
}

impl PersistentTaskType {
    pub async fn run_resolve_native<B: Backend + 'static>(
        fn_id: FunctionId,
        inputs: Vec<TaskInput>,
        turbo_tasks: Arc<dyn TurboTasksBackendApi<B>>,
    ) -> Result<RawVc> {
        let mut resolved_inputs = Vec::with_capacity(inputs.len());
        for input in inputs.into_iter() {
            resolved_inputs.push(input.resolve().await?)
        }
        Ok(turbo_tasks.native_call(fn_id, resolved_inputs))
    }

    pub async fn run_resolve_trait<B: Backend + 'static>(
        trait_type: TraitTypeId,
        name: Cow<'static, str>,
        inputs: Vec<TaskInput>,
        turbo_tasks: Arc<dyn TurboTasksBackendApi<B>>,
    ) -> Result<RawVc> {
        let mut resolved_inputs = Vec::with_capacity(inputs.len());
        let mut iter = inputs.into_iter();
        if let Some(this) = iter.next() {
            let this = this.resolve().await?;
            let this_value = this.clone().resolve_to_value().await?;
            match this_value.get_trait_method(trait_type, name) {
                Ok(native_fn) => {
                    resolved_inputs.push(this);
                    for input in iter {
                        resolved_inputs.push(input)
                    }
                    Ok(turbo_tasks.dynamic_call(native_fn, resolved_inputs))
                }
                Err(name) => {
                    if !this_value.has_trait(trait_type) {
                        let traits = this_value
                            .traits()
                            .iter()
                            .map(|t| format!(" {}", t))
                            .collect::<String>();
                        Err(anyhow!(
                            "{} doesn't implement {} (only{})",
                            this_value,
                            registry::get_trait(trait_type),
                            traits,
                        ))
                    } else {
                        Err(anyhow!(
                            "{} implements trait {}, but method {} is missing",
                            this_value,
                            registry::get_trait(trait_type),
                            name
                        ))
                    }
                }
            }
        } else {
            panic!("No arguments for trait call");
        }
    }

    pub fn run<B: Backend + 'static>(
        self,
        turbo_tasks: Arc<dyn TurboTasksBackendApi<B>>,
    ) -> Pin<Box<dyn Future<Output = Result<RawVc>> + Send>> {
        match self {
            PersistentTaskType::Native(fn_id, inputs) => {
                let native_fn = registry::get_function(fn_id);
                let bound = native_fn.bind(&inputs);
                (bound)()
            }
            PersistentTaskType::ResolveNative(fn_id, inputs) => {
                Box::pin(Self::run_resolve_native(fn_id, inputs, turbo_tasks))
            }
            PersistentTaskType::ResolveTrait(trait_type, name, inputs) => Box::pin(
                Self::run_resolve_trait(trait_type, name, inputs, turbo_tasks),
            ),
        }
    }
}
