use std::{
    future::Future,
    mem,
    sync::{Arc, Mutex},
};

use wasm_set_stack_pointer;

pub const STACK_ALIGN: usize = 1024 * 64;
static mut TLS_SIZE: usize = 0;

#[wasm_bindgen]
pub unsafe fn run_as_worker(worker_ptr: u32) -> Result<(), JsValue> {
    let mut worker = ptr as *mut Worker;
    loop {
        if (*worker).work != 0 {
            let work = Box::from_raw((*worker).work as *mut Work);
            (work.func)(work.scope);
            (*worker).work = 0;
        }
        (*worker).available = 1;
        atomics::memory_atomic_wait32(&mut (*worker).available as *mut i32, 1, -1);
    }
}

struct Work {
    func: Box<dyn FnOnce() + Send>,
}

/// Used to create a TaskPool
#[derive(Debug, Default, Clone)]
pub struct TaskPoolBuilder {
    /// If set, we'll set up the thread pool to use at most n workers. Otherwise use
    /// the logical core count of the system
    num_workers: Option<usize>,
    /// If set, we'll use the given stack size rather than the system default
    stack_size: Option<usize>,
    /// Allows customizing the name of the workers - helpful for debugging. If set, workers will
    /// be named <worker_name> (<worker_index>), i.e. "MyWorkerPool (2)"
    worker_name: Option<String>,
}

impl TaskPoolBuilder {
    /// Creates a new TaskPoolBuilder instance
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the number of workers created for the pool. If unset, we default to the number
    /// of logical cores of the system
    pub fn num_workers(mut self, num_workers: usize) -> Self {
        self.num_workers = Some(num_workers);
        self
    }

    /// Override the stack size of the workers created for the pool
    pub fn stack_size(mut self, stack_size: usize) -> Self {
        self.stack_size = Some(stack_size);
        self
    }

    /// Override the name of the workers created for the pool. If set, workers will
    /// be named <worker_name> (<worker_index>), i.e. "MyWorkerPool (2)"
    pub fn worker_name(mut self, worker_name: String) -> Self {
        self.worker_name = Some(worker_name);
        self
    }

    /// Creates a new ThreadPoolBuilder based on the current options.
    pub fn build(self) -> TaskPool {
        TaskPool::new_internal(
            self.num_workers,
            self.stack_size,
            self.worker_name.as_deref(),
        )
    }
}

#[derive(Debug)]
struct WorkerPool {
    workers: Mutex<Vec<Worker>>,
}

impl WorkerPool {
    fn available_worker(&self) -> Result<usize, String> {
        let workers = self.workers.lock();
        for (idx, worker) in workers.iter().enumerate() {
            if worker.available == 1 {
                return Ok(idx);
            }
        }

        // TODO: Spawn new worker if one is not available
    }

    fn execute<'scope, F, T>(&self, f: F, scope: &mut S)
    where
        S: Scope<'scope, T>,
        F: FnOnce(&mut S) + 'scope + Send
    {
        let worker = self.available_worker()?;
        let mut workers = self.workers.lock();
        assert_eq!(workers[worker].available, 1);
        let work = Box::new(Work {
            func: Box::new(f),
        });
        let work_ptr = Box::into_raw(work);
        workers[worker].available = 0;
        workers[worker].work = work_ptr as u32;
        unsafe {
            atomics::memory_atomic_notify(workers[worker].available as *mut i32, 1);
        }
        Ok(())
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        for worker in self.workers.drain(..) {
            worker.terminate();
        }
    }
}

/// A pool for executing tasks. Tasks are futures that are being automatically driven by
/// the pool on web workers owned by the pool.
#[derive(Debug, Default, Clone)]
pub struct TaskPool {
    /// The executor for the pool
    ///
    /// This has to be separate from WorkerPool because we have to create an Arc<Executor> to
    /// pass into the worker threads, and we must create the worker threads before we can create
    /// the Vec<Task<T>> contained within WorkerPool
    executor: Arc<async_executor::Executor<'static>>,

    /// Inner state of the pool
    worker_pool: Arc<WorkerPool>,
}

impl TaskPool {
    /// Create a `TaskPool` with the default configuration.
    pub fn new() -> Self {
        TaskPoolBuilder::new().build()
    }

    fn new_internal(
        num_workers: Option<usize>,
        stack_size: Option<usize>,
        worker_name: Option<&str>,
    ) -> Self {
        let executor = Arc::new(async_executor::Executor::new());

        let window = web_sys::window();
        let num_workers = num_workers.unwrap_or_else(window.navigator().hardware_concurrency());

        let workers = (0..num_workers)
            .map(|i| {
                let ex = Arc::clone(&executor);

                let worker_name = if let Some(worker_name) = worker_name {
                    format!("{} ({})", worker_name, i)
                } else {
                    format!("TaskPool ({})", i)
                };

                let worker_builder = WorkerBuilder::new().name(worker_name);

                if let Some(stack_size) = stack_size {
                    worker_builder = worker_builder.stack_size(stack_size);
                }

                worker_builder.spawn()
            })
            .collect();

        Self {
            executor,
            worker_pool: Arc::new(WorkerPool {
                workers,
            }),
        }
    }

    /// Return the number of threads owned by the task pool
    pub fn thread_num(&self) -> usize {
        self.worker_pool.workers.len()
    }

    /// Allows spawning non-`static futures on the thread pool. The function takes a callback,
    /// passing a scope object into it. The scope object provided to the callback can be used
    /// to spawn tasks. This function will await the completion of all tasks before returning.
    ///
    /// This is similar to `rayon::scope` and `crossbeam::scope`
    pub fn scope<'scope, F, T>(&self, f: F) -> Vec<T>
    where
        F: FnOnce(&mut Scope<'scope, T>) + 'scope + Send,
        T: Send + 'static,
    {

        // TODO: May need to wrap the execution in a future which can be driven by the executor, in order to
        // obtain the result value of the execution
        // TODO: self.worker_pool.execute(f, &mut scope);

        // let executor: &async_executor::Executor = &*self.executor;
        // let executor: &'scope async_executor::Executor = unsafe { mem::transmute(executor) };
        // let local_executor: &'scope async_executor::LocalExecutor =
        //     unsafe { mem::transmute(local_executor) };
        // let mut scope = Scope {
        //     executor,
        //     local_executor,
        //     spawned: Vec::new(),
        // };

        // f(&mut scope);

        // if scope.spawned.is_empty() {
        //     Vec::default()
        // } else if scope.spawned.len() == 1 {
        //     vec![future::block_on(&mut scope.spawned[0])]
        // } else {
        //     let fut = async move {
        //         let mut results = Vec::with_capacity(scope.spawned.len());
        //         for task in scope.spawned {
        //             results.push(task.await);
        //         }

        //         results
        //     };

        //     // Pin the futures on the stack.
        //     pin!(fut);

        //     // SAFETY: This function blocks until all futures complete, so we do not read/write
        //     // the data from futures outside of the 'scope lifetime. However,
        //     // rust has no way of knowing this so we must convert to 'static
        //     // here to appease the compiler as it is unable to validate safety.
        //     let fut: Pin<&mut (dyn Future<Output = Vec<T>>)> = fut;
        //     let fut: Pin<&'static mut (dyn Future<Output = Vec<T>> + 'static)> =
        //         unsafe { mem::transmute(fut) };

        //     // The thread that calls scope() will participate in driving tasks in the pool
        //     // forward until the tasks that are spawned by this scope() call
        //     // complete. (If the caller of scope() happens to be a thread in
        //     // this thread pool, and we only have one thread in the pool, then
        //     // simply calling future::block_on(spawned) would deadlock.)
        //     let mut spawned = local_executor.spawn(fut);
        //     loop {
        //         if let Some(result) = future::block_on(future::poll_once(&mut spawned)) {
        //             break result;
        //         };

        //         self.executor.try_tick();
        //         local_executor.try_tick();
        //     }
        // }
    }

    // Spawns a static future onto the JS event loop. For now it is returning FakeTask
    // instance with no-op detach method. Returning real Task is possible here, but tricky:
    // future is running on JS event loop, Task is running on async_executor::LocalExecutor
    // so some proxy future is needed. Moreover currently we don't have long-living
    // LocalExecutor here (above `spawn` implementation creates temporary one)
    // But for typical use cases it seems that current implementation should be sufficient:
    // caller can spawn long-running future writing results to some channel / event queue
    // and simply call detach on returned Task (like AssetServer does) - spawned future
    // can write results to some channel / event queue.

    pub fn spawn<T>(&self, future: impl Future<Output = T> + 'static) -> Task<T>
    where
        T: 'static,
    {
        Task::new(self.executor.spawn(future))
    }

    pub fn spawn_local<T>(&self, future: impl Future<Output = T> + 'static) -> Task<T>
    where
        T: 'static,
    {
        Task::new(TaskPool::LOCAL_EXECUTOR.with(|executor| executor.spawn(future)))
    }
}


#[derive(Debug)]
pub struct Scope<'scope, T> {
    executor: &'scope async_executor::Executor<'scope>,
    local_executor: &'scope async_executor::LocalExecutor<'scope>,
    spawned: Vec<async_executor::Task<T>>,
}

impl<'scope, T: Send + 'scope> Scope<'scope, T> {
    pub fn spawn<Fut: Future<Output = T> + 'scope + Send>(&mut self, f: Fut) {
        let task = self.executor.spawn(f);
        self.spawned.push(task);
    }

    pub fn spawn_local<Fut: Future<Output = T> + 'scope>(&mut self, f: Fut) {
        let task = self.local_executor.spawn(f);
        self.spawned.push(task);
    }
}

// #[derive(Debug)]
// pub struct FakeTask;

// impl FakeTask {
//     pub fn detach(self) {}
// }

// #[derive(Debug)]
// pub struct Scope<'scope, T> {
//     executor: &'scope async_executor::LocalExecutor<'scope>,
//     // Vector to gather results of all futures spawned during scope run
//     results: Vec<Arc<Mutex<Option<T>>>>,
// }

// impl<'scope, T: Send + 'scope> Scope<'scope, T> {
//     pub fn spawn<Fut: Future<Output = T> + 'scope + Send>(&mut self, f: Fut) {
//         self.spawn_local(f);
//     }

//     pub fn spawn_local<Fut: Future<Output = T> + 'scope>(&mut self, f: Fut) {
//         let result = Arc::new(Mutex::new(None));
//         self.results.push(result.clone());
//         let f = async move {
//             result.lock().unwrap().replace(f.await);
//         };
//         self.executor.spawn(f).detach();
//     }
// }

// Used to create a Web Worker
#[derive(Default)]
pub struct WorkerMemory {
    stack: *const u8,
    tls: *const u8,
}

impl WorkerMemory {
    /// Creates a new WorkerMemory instance
    pub fn new() -> Self {
        Self::default()
    }
}

// Used to create a Web Worker
#[derive(Default)]
pub struct Worker {
    name: String,
    memory: WorkerMemory,
    worker: web_sys::Worker,
    work: u32,
    available: i32
}

impl Worker {
    pub static WEB_WORKER_SOURCE: &str = "
    let wasm
    let initialized = false

    self.onmessage = async function({ binary, memory, stack, tls, worker }) {
        if (!initialized) {
            wasm = await WebAssembly.instantiate(binary, {
                env: {
                    ...imports,
                    memory
                }
            })
    
            wasm.exports.set_stack_pointer(stack)
            wasm.exports.__wasm_init_tls(tls)
            
            initialized = true
        }

        wasm.exports.run_as_worker(worker)
    }
    ";

    /// Creates a new Worker instance
    pub fn new() -> Self {
        Self::default()
    }

    pub fn name(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    pub fn memory(mut self, memory: WorkerMemory) -> Self {
        self.memory = memory;
        self
    }

    pub fn worker(mut self, worker: web_sys::Worker) -> Self {
        self.worker = worker;
        self
    }

    pub fn init(mut self) {
        let init_message = js_sys::Object::new();
        js_sys::Reflect::set(&init_message, &"binary".into(), &"should be a reference to the current module passed from set_current_module in index.html".into());
        js_sys::Reflect::set(&init_message, &"memory".into(), wasm_bindgen::memory());
        js_sys::Reflect::set(&init_message, &"stack".into(), self.memory.stack);
        js_sys::Reflect::set(&init_message, &"tls".into(), self.memory.tls);
        js_sys::Reflect::set(&init_message, &"worker".into(), worker as *const Worker as u32);

        worker.post_message(&init_message);
    }
}

// Used to create a Web Worker
#[derive(Default)]
pub struct WorkerBuilder {
    name: Option<String>,
    stack_size: Option<usize>,
}

impl WorkerBuilder {
    /// Creates a new WorkerBuilder instance
    pub fn new() -> Self {
        Self::default()
    }

    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn stack_size(mut self, size: usize) -> Self {
        self.stack_size = Some(size);
        self
    }

    pub fn spawn(self) -> Worker {
        let memory = WorkerMemory::new();
        let stack_size = self.stack_size.unwrap_or_else(usize::DEFAULT_MIN_STACK_SIZE);
        unsafe {
            let stack_layout = core::alloc::Layout::from_size_align(stack_size, STACK_ALIGN).unwrap();
            let tls_layout = core::alloc::Layout::from_size_align(TLS_SIZE as usize, 8).unwrap();
            memory.stack = std::alloc::alloc(stack_layout).offset(stack_size as isize);
            memory.tls = std::alloc::alloc(tls_layout);
        }

        let blob = web_sys::Blob::new_with_str_sequence_and_options(
            js_sys::Array::of1(JsValue::from_str(Worker::WEB_WORKER_SOURCE)),
            web_sys::BlobPropertyBag::new().type_("text/javascript")
        );
        let blob_url = web_sys::Url::create_object_url_with_blob(blob);

        let worker_options = web_sys::WorkerOptions::new().name(self.name.as_deref());
        let web_worker = web_sys::Worker::new_with_options(blob_url, worker_options);

        let worker = Worker::new()
            .name(self.name)
            .memory(memory)
            .worker(web_worker);

        worker.init();

        worker
    }
}