export class ResourceError extends Error {
  constructor(
    readonly code: string,
    message: string,
    cause?: unknown,
  ) {
    super(message, { cause });
    this.name = "ResourceError";
  }
}

export class ResourceOwnershipError extends ResourceError {
  constructor(message: string, cause?: unknown) {
    super("ownership", message, cause);
    this.name = "ResourceOwnershipError";
  }
}

function failure(error: unknown): ResourceError {
  if (error instanceof ResourceError) return error;
  const code =
    typeof error === "object" &&
    error !== null &&
    "pulsebeamCode" in error &&
    typeof error.pulsebeamCode === "string"
      ? error.pulsebeamCode
      : "runtime";
  const message = error instanceof Error ? error.message : String(error);
  return code === "ownership"
    ? new ResourceOwnershipError(message, error)
    : new ResourceError(code, message, error);
}

export interface ResourceRuntime {
  resource(command: unknown): Promise<unknown>;
}
export interface ResourceBinding {
  readonly runtime: ResourceRuntime;
  readonly owner: bigint;
  readonly slot?: string;
}
export interface ResourceObserver {
  bind(binding: ResourceBinding): void;
  unbind(): void;
  error?(error: ResourceError): void;
}

/** Host attachment and Promise lifetimes only. Core grants and releases owners. */
export class ResourceOwners {
  #runtime: ResourceRuntime | undefined;
  #closed = false;
  readonly #owners = new Set<ResourceOwner>();
  constructor(readonly report: (error: ResourceError) => void) {}

  acquire(command: unknown, observer: ResourceObserver): ResourceOwner {
    if (this.#closed) throw new ResourceError("closed", "agent is closed");
    const owner = new ResourceOwner(this, command, observer);
    this.#owners.add(owner);
    if (this.#runtime) owner.activate(this.#runtime);
    return owner;
  }
  current(runtime: ResourceRuntime): boolean {
    return !this.#closed && runtime === this.#runtime;
  }
  activate(runtime: ResourceRuntime): void {
    if (this.#closed || this.#runtime === runtime) return;
    this.deactivate();
    this.#runtime = runtime;
    for (const owner of this.#owners) owner.activate(runtime);
  }
  deactivate(): void {
    this.#runtime = undefined;
    for (const owner of this.#owners) owner.detach();
  }
  forget(owner: ResourceOwner): void {
    this.#owners.delete(owner);
  }
  close(): void {
    this.#closed = true;
    this.#runtime = undefined;
    for (const owner of this.#owners) void owner.close();
  }
}

export class ResourceOwner {
  #binding: ResourceBinding | undefined;
  #attachment: object | undefined;
  #closed = false;
  #closing: Promise<void> | undefined;
  readonly #pending = new Set<Promise<void>>();
  readonly ready: Promise<void>;
  #resolve!: () => void;
  #reject!: (error: ResourceError) => void;

  constructor(
    private registry: ResourceOwners,
    private command: unknown,
    private observer: ResourceObserver,
  ) {
    this.ready = new Promise((resolve, reject) => {
      this.#resolve = resolve;
      this.#reject = reject;
    });
    void this.ready.catch(() => {});
  }
  get binding(): ResourceBinding | undefined {
    return this.#binding;
  }
  current(binding: ResourceBinding): boolean {
    return (
      !this.#closed &&
      binding === this.#binding &&
      this.registry.current(binding.runtime)
    );
  }
  activate(runtime: ResourceRuntime): void {
    const attachment = {};
    this.#attachment = attachment;
    this.#track(this.#acquire(runtime, attachment));
  }
  async #acquire(runtime: ResourceRuntime, attachment: object): Promise<void> {
    try {
      const result = await runtime.resource(this.command);
      if (
        !result ||
        typeof result !== "object" ||
        !("owner" in result) ||
        typeof result.owner !== "bigint"
      )
        throw new ResourceError("runtime", "invalid core resource owner");
      const binding = {
        runtime,
        owner: result.owner,
        ...("slot" in result && typeof result.slot === "string"
          ? { slot: result.slot }
          : {}),
      };
      if (
        this.#closed ||
        attachment !== this.#attachment ||
        !this.registry.current(runtime)
      ) {
        await this.#release(binding);
        return;
      }
      this.#binding = binding;
      this.observer.bind(binding);
      this.#resolve();
    } catch (error) {
      if (
        !this.#closed &&
        attachment === this.#attachment &&
        this.registry.current(runtime)
      )
        this.fail(error);
    }
  }
  fail(error: unknown): void {
    if (this.#closed) return;
    const value = failure(error);
    this.#reject(value);
    void this.close();
    (this.observer.error ?? this.registry.report)(value);
  }
  detach(): void {
    this.#attachment = undefined;
    const binding = this.#binding;
    this.#binding = undefined;
    this.observer.unbind();
    if (binding) this.#track(this.#release(binding));
  }
  async #release(binding: ResourceBinding): Promise<void> {
    try {
      await binding.runtime.resource({ op: "release", owner: binding.owner });
    } catch {
      /* A stopped runtime has already torn down its physical resources. */
    }
  }
  #track(promise: Promise<void>): void {
    this.#pending.add(promise);
    void promise.then(
      () => this.#pending.delete(promise),
      () => this.#pending.delete(promise),
    );
  }
  close(): Promise<void> {
    if (this.#closing) return this.#closing;
    this.#closed = true;
    this.detach();
    this.registry.forget(this);
    this.#reject(new ResourceError("closed", "resource owner is closed"));
    return (this.#closing = Promise.all([...this.#pending]).then(() => {}));
  }
}
