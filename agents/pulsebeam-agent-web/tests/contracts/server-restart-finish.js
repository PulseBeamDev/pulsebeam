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
