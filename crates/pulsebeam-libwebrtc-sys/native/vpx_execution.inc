// Included inside execution.cc's private namespace, after the kernel declarations.
// Only the pinned Linux x86_64 VP8 C entries are eligible. This is a runtime
// continuation service, not a codec implementation or a second ready scheduler.
#if defined(__linux__) && defined(__x86_64__) && !defined(__ANDROID__)
class NativeVpxRuntime;
thread_local NativeVpxRuntime* active_vpx_runtime = nullptr;
VPxWorkerInterface native_vpx_workers{};

class NativeVpxRuntime final {
  struct Worker;
  struct Waiter {
    Worker* worker;
    bool ready = false;
    Waiter* next = nullptr;
  };
  struct Semaphore {
    unsigned tokens;
    Waiter* first = nullptr;
    Waiter* last = nullptr;
    std::shared_ptr<NativeBuiltinVp8Execution> execution;
    std::shared_ptr<NativeTaskQueueFactory::State> queues;
  };
  struct Worker final : webrtc::YieldInterface {
    NativeVpxRuntime& runtime;
    VPxWorker* native;
    std::shared_ptr<NativeBuiltinVp8Execution> execution;
    std::shared_ptr<NativeTaskQueueFactory::State> queues;
    std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter> queue;
    ucontext_t context{};
    void* mapping = MAP_FAILED;
    size_t mapping_size = 0;
    void* stack = nullptr;
    size_t stack_size = 0;
    void* fake_stack = nullptr;
    webrtc::TaskQueueBase* saved_queue = nullptr;
    int saved_errno = 0;
    bool started = false;
    bool queued = false;
    bool done = false;
    Worker(NativeVpxRuntime& runtime, VPxWorker* native,
        std::shared_ptr<NativeBuiltinVp8Execution> execution,
        const std::shared_ptr<NativeTaskQueueFactory::State>& queues)
        : runtime(runtime), native(native), execution(std::move(execution)), queues(queues),
          queue(new CooperativeTaskQueue(queues, "native-vp8-worker")) {}
    ~Worker() override {
      RTC_CHECK(!queued && (!started || done));
      if (mapping != MAP_FAILED) RTC_CHECK_EQ(munmap(mapping, mapping_size), 0);
    }
    bool Allocate() {
      // Match the native pthread default stack resource, without creating a
      // pthread. Guard both boundaries; valid codec accesses are unchanged.
      pthread_attr_t attributes;
      if (pthread_attr_init(&attributes) != 0) return false;
      const int error = pthread_attr_getstacksize(&attributes, &stack_size);
      pthread_attr_destroy(&attributes);
      const long page = sysconf(_SC_PAGESIZE);
      if (error || page <= 0 || stack_size == 0 || stack_size % page ||
          stack_size > SIZE_MAX - 2 * static_cast<size_t>(page)) return false;
      mapping_size = stack_size + 2 * page;
      mapping = mmap(nullptr, mapping_size, PROT_NONE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
      if (mapping == MAP_FAILED) return false;
      stack = static_cast<char*>(mapping) + page;
      if (mprotect(stack, stack_size, PROT_READ | PROT_WRITE) != 0 ||
          getcontext(&context) != 0) return false;
      context.uc_stack.ss_sp = stack;
      context.uc_stack.ss_size = stack_size;
      context.uc_link = nullptr;
      const uintptr_t address = reinterpret_cast<uintptr_t>(this);
      makecontext(&context, reinterpret_cast<void (*)()>(&Enter), 2,
                  uint32_t(address), uint32_t(address >> 32));
      return true;
    }
    static void Enter(uint32_t low, uint32_t high) {
      auto& worker = *reinterpret_cast<Worker*>(uintptr_t(low) | (uintptr_t(high) << 32));
      auto& runtime = worker.runtime;
      FinishSwitch(worker.fake_stack, &runtime.root_bottom_, &runtime.root_size_);
      errno = worker.saved_errno;
      // Executes the real persistent entry and its SDK-owned C call frames.
      native_vpx_workers.execute(worker.native);
      worker.done = true;
      worker.native->status_ = VPX_WORKER_STATUS_OK;
      StartSwitch(nullptr, runtime.root_bottom_, runtime.root_size_);
      RTC_CHECK_EQ(swapcontext(&worker.context, &runtime.root_), 0);
      RTC_CHECK_NOTREACHED();
    }
    void Enqueue() {
      RTC_CHECK(!done);
      if (queued) return;
      queued = true;
      queue->PostTask([this] { Resume(); });
    }
    void Resume() {
      RTC_CHECK(queued && !done && runtime.current_ == nullptr);
      queued = false;
      const int root_errno = errno;
      {
        NativeBuiltinVp8Scope context_scope(execution);
        CooperativeScope cooperative_scope(nullptr);
        CooperativeTaskQueue::WorkerScope queue_scope(saved_queue);
        webrtc::ScopedYieldPolicy yield_scope(this);
        runtime.current_ = this;
        StartSwitch(&runtime.fake_stack_, stack, stack_size);
        RTC_CHECK_EQ(swapcontext(&runtime.root_, &context), 0);
        FinishSwitch(runtime.fake_stack_);
        runtime.current_ = nullptr;
      }
      errno = root_errno;
    }
    void Suspend(bool ready) {
      RTC_CHECK(runtime.current_ == this && !done);
      saved_errno = errno;
      saved_queue = webrtc::TaskQueueBase::Current();
      if (ready) Enqueue();
      StartSwitch(&fake_stack, runtime.root_bottom_, runtime.root_size_);
      RTC_CHECK_EQ(swapcontext(&context, &runtime.root_), 0);
      FinishSwitch(fake_stack);
      errno = saved_errno;
      RTC_CHECK(runtime.current_ == this);
    }
    void YieldExecution() override { Suspend(true); }
  };

 public:
  explicit NativeVpxRuntime(std::shared_ptr<NativeRuntimeRetirement> owner)
      : owner_(std::move(owner)), previous_runtime_(*vpx_get_runtime_interface()) {
    RTC_CHECK(active_vpx_runtime == nullptr);
    native_vpx_workers = *vpx_get_worker_interface();
    active_vpx_runtime = this;
    const VPxWorkerInterface workers = {Init, Reset, Sync, Launch, Execute, End};
    const VPxRuntimeInterface runtime = {
        Enabled, SemInit, SemWait, SemPost, SemDestroy, Yield, AvailableProcessors};
    RTC_CHECK(vpx_set_worker_interface(&workers));
    RTC_CHECK(vpx_set_runtime_interface(&runtime));
  }
  ~NativeVpxRuntime() {
    RTC_CHECK(active_vpx_runtime == this && current_ == nullptr);
    RTC_CHECK(workers_.empty() && semaphores_.empty());
    RTC_CHECK(vpx_set_runtime_interface(
        previous_runtime_.enabled ? &previous_runtime_ : nullptr));
    RTC_CHECK(vpx_set_worker_interface(&native_vpx_workers));
    active_vpx_runtime = nullptr;
  }

 private:
  static void StartSwitch(void** save, const void* bottom, size_t size) {
#if defined(PULSEBEAM_ASAN_FIBERS)
    __sanitizer_start_switch_fiber(save, bottom, size);
#else
    (void)save; (void)bottom; (void)size;
#endif
  }
  static void FinishSwitch(void* save, const void** old_bottom = nullptr,
                           size_t* old_size = nullptr) {
#if defined(PULSEBEAM_ASAN_FIBERS)
    __sanitizer_finish_switch_fiber(save, old_bottom, old_size);
#else
    (void)save; (void)old_bottom; (void)old_size;
#endif
  }
  static NativeBuiltinVp8Execution::State* Context() {
    auto* runtime = active_vpx_runtime;
    auto* context = current_builtin_vp8;
    return runtime && context && context->owner == runtime->owner_
        ? context : nullptr;
  }
  static std::shared_ptr<NativeBuiltinVp8Execution> Capture(
      const NativeBuiltinVp8Execution::State& context) {
    return std::make_shared<NativeBuiltinVp8Execution>(
        std::make_unique<NativeBuiltinVp8Execution::State>(context));
  }
  static bool PumpOne(const std::shared_ptr<NativeTaskQueueFactory::State>& queues) {
    const int saved_errno = errno;
    auto* caller = webrtc::TaskQueueBase::Current();
    suspended_queues.push_back(caller);
    const auto progress = PumpClock(queues->clock, 1, caller, true);
    suspended_queues.pop_back();
    errno = saved_errno;
    return progress != 0;
  }
  static int Enabled() { return Context() != nullptr; }
  static int AvailableProcessors() {
    auto* context = Context();
    if (!context) return 0;
    context->construction_failed = false;
    return context->cores;
  }
  static Worker* Find(VPxWorker* worker) {
    auto* runtime = active_vpx_runtime;
    if (!runtime) return nullptr;
    auto found = runtime->workers_.find(worker);
    return found == runtime->workers_.end() ? nullptr : found->second.get();
  }
  static void Init(VPxWorker* worker) {
    RTC_CHECK(Find(worker) == nullptr);
    native_vpx_workers.init(worker);
  }
  static int Reset(VPxWorker* worker) {
    if (auto* existing = Find(worker)) return Sync(existing->native);
    auto* context = Context();
    if (!context) return native_vpx_workers.reset(worker);
    if (context->construction_failed) return 0;
    const int saved_errno = errno;
    auto state = std::make_unique<Worker>(*active_vpx_runtime, worker,
                                         Capture(*context), context->queues);
    const bool allocated = state->Allocate();
    errno = saved_errno;
    if (!allocated) return 0;  // Native create error, never pthread fallback.
    worker->status_ = VPX_WORKER_STATUS_OK;
    worker->had_error = 0;
    active_vpx_runtime->workers_.emplace(worker, std::move(state));
    return 1;
  }
  static int Sync(VPxWorker* worker) {
    auto* state = Find(worker);
    if (!state) return native_vpx_workers.sync(worker);
    while (state->started && !state->done) {
      RTC_CHECK(PumpOne(state->queues));
    }
    return !worker->had_error;
  }
  static void Launch(VPxWorker* worker) {
    auto* state = Find(worker);
    if (!state) { native_vpx_workers.launch(worker); return; }
    RTC_CHECK(!state->started);
    state->started = true;
    worker->status_ = VPX_WORKER_STATUS_WORKING;
    state->Enqueue();
  }
  static void Execute(VPxWorker* worker) { native_vpx_workers.execute(worker); }
  static void End(VPxWorker* worker) {
    auto* state = Find(worker);
    if (!state) { native_vpx_workers.end(worker); return; }
    Sync(worker);
    worker->status_ = VPX_WORKER_STATUS_NOT_OK;
    active_vpx_runtime->workers_.erase(worker);
  }
  static Semaphore* FindSem(void* address) {
    auto* runtime = active_vpx_runtime;
    if (!runtime) return nullptr;
    auto found = runtime->semaphores_.find(address);
    return found == runtime->semaphores_.end() ? nullptr : found->second.get();
  }
  static int SemInit(void* address, int pshared, unsigned value, int* result) {
    auto* context = Context();
    if (!context) return 0;
    const int saved_errno = errno;
    RTC_CHECK(FindSem(address) == nullptr);
    // Only encoder-specific registration reaches this callback. On admission
    // failure leave a real initialized semaphore for native partial cleanup,
    // but make the subsequent worker reset fail instead of spawning a pthread.
    auto state = std::unique_ptr<Semaphore>(new (std::nothrow) Semaphore{value, nullptr, nullptr, Capture(*context), context->queues});
    if (pshared != 0 || value > SEM_VALUE_MAX || !state) {
      context->construction_failed = true;
      errno = saved_errno;
      return 0;
    }
    active_vpx_runtime->semaphores_.emplace(address, std::move(state));
    errno = saved_errno;
    *result = 0;
    return 1;
  }
  static int SemWait(void* address, int* result) {
    auto* sem = FindSem(address);
    if (!sem) return 0;
    const int saved_errno = errno;
    if (sem->tokens) --sem->tokens;
    else {
      // The waiter lives in the real suspended C-call continuation or caller
      // stack. FIFO wake reserves its token; a later waiter cannot steal it.
      Waiter waiter{active_vpx_runtime->current_};
      if (sem->last) sem->last->next = &waiter; else sem->first = &waiter;
      sem->last = &waiter;
      while (!waiter.ready) {
        if (waiter.worker) waiter.worker->Suspend(false);
        else RTC_CHECK(PumpOne(sem->queues));
      }
    }
    errno = saved_errno;
    *result = 0;
    return 1;
  }
  static int SemPost(void* address, int* result) {
    auto* sem = FindSem(address);
    if (!sem) return 0;
    const int saved_errno = errno;
    if (sem->first) {
      auto* waiter = sem->first;
      sem->first = waiter->next;
      if (!sem->first) sem->last = nullptr;
      waiter->ready = true;
      if (waiter->worker) waiter->worker->Enqueue();
    } else {
      if (sem->tokens == SEM_VALUE_MAX) {
        errno = EOVERFLOW; *result = -1; return 1;
      }
      ++sem->tokens;
    }
    errno = saved_errno;
    *result = 0;
    return 1;
  }
  static int SemDestroy(void* address, int* result) {
    auto* sem = FindSem(address);
    if (!sem) return 0;
    RTC_CHECK(sem->first == nullptr);
    active_vpx_runtime->semaphores_.erase(address);
    *result = 0;
    return 1;
  }
  static void Yield() {
    auto* context = Context();
    if (!context) return;
    if (auto* worker = active_vpx_runtime->current_) worker->Suspend(true);
    else RTC_CHECK(PumpOne(context->queues));
  }
  std::shared_ptr<NativeRuntimeRetirement> owner_;
  VPxRuntimeInterface previous_runtime_;
  std::map<VPxWorker*, std::unique_ptr<Worker>> workers_;
  std::map<void*, std::unique_ptr<Semaphore>> semaphores_;
  Worker* current_ = nullptr;
  ucontext_t root_{};
  void* fake_stack_ = nullptr;
  const void* root_bottom_ = nullptr;
  size_t root_size_ = 0;
};
#endif
