(async () => {
  const { sender, receiver, stream, pumping, wait } =
    globalThis.__serverRestart;
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
    const failure = await pumping;
    if (
      !(failure instanceof window.pulsebeam.OperationError) ||
      failure.info.code !== window.pulsebeam.ErrorCode.ConnectionFailed
    ) {
      throw new Error(
        "expired ownership did not terminate reception with its shared cause",
      );
    }
    await stream.close();
    try {
      await stream.next();
      throw new Error("close erased the terminal receive failure");
    } catch (error) {
      if (error !== failure) throw error;
    }
    for (const agent of [sender, receiver]) {
      const state = agent.state();
      if (
        state.error?.code !== window.pulsebeam.ErrorCode.ConnectionFailed ||
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
    await stream.close();
    await pumping;
    await Promise.all([sender.close(), receiver.close()]);
    delete globalThis.__serverRestart;
  }
})();
