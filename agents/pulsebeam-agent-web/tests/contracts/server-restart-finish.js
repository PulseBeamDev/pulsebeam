(async () => {
  const { AgentError, ErrorCode } = window.pulsebeam;
  const terminal = (failure) =>
    failure != null &&
    AgentError.Operation.instanceOf(failure) &&
    failure.inner.info.code === ErrorCode.ConnectionFailed;
  const { sender, receiver, stream, pumping, wait } =
    globalThis.__serverRestart;
  const bounded = async (work, label) => {
    let timer;
    try {
      return await Promise.race([
        work,
        new Promise((_, reject) => {
          timer = setTimeout(
            () => reject(new Error(`Timed out: ${label}`)),
            5000,
          );
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  };
  try {
    await Promise.all(
      [sender, receiver].map((agent) =>
        wait(
          agent,
          (state) => state.status === window.pulsebeam.AgentStatus.Failed,
          "expired ownership after SFU restart",
        ),
      ),
    );
    const failure = await bounded(
      pumping,
      "pending data read after terminal Agent state",
    );
    if (!terminal(failure)) {
      throw new Error(
        "expired ownership did not terminate reception with its shared cause",
      );
    }
    await bounded(stream.close(), "terminal data subscription close");
    try {
      await bounded(stream.next(), "post-close terminal data read");
      throw new Error("close erased the terminal receive failure");
    } catch (error) {
      if (!terminal(error)) throw error;
    }
    for (const agent of [sender, receiver]) {
      const state = agent.state();
      if (
        state.error?.code !== ErrorCode.ConnectionFailed ||
        state.room !== undefined ||
        state.connectionId !== undefined
      ) {
        throw new Error(
          `expired owner retained admission: code=${state.error?.code} room=${state.room} connection=${state.connectionId}`,
        );
      }
    }
    return true;
  } finally {
    try {
      await bounded(
        Promise.all([sender.close(), receiver.close(), stream.close()]),
        "restart fixture cleanup",
      );
    } finally {
      delete globalThis.__serverRestart;
    }
  }
})();
