new Promise((resolve) => { const poll = () => globalThis.__pulsebeamReactObservation ? resolve(globalThis.__pulsebeamReactObservation) : setTimeout(poll, 10); poll(); })
