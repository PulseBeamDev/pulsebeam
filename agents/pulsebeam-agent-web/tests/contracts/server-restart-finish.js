(async () => {
  const { sender, receiver, stream, pumping, wait } =
    globalThis.__serverRestart;
  try {
    await Promise.all(
      [sender, receiver].map((agent) =>
        wait(
          agent,
          (snapshot) => snapshot.connection === "terminal-failure",
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
    return [sender, receiver].every(
      (agent) =>
        agent.error?.class === "resource-expired" &&
        agent.snapshot().credentialOutcome.status === "uncertain",
    );
  } finally {
    await stream.close();
    await pumping;
    await Promise.all([sender.close(), receiver.close()]);
    delete globalThis.__serverRestart;
  }
})();
