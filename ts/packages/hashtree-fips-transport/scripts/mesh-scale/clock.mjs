// Advance the simulated links only after the previous instant's promises settle.
export class SimulationClock {
  now = 1;
  #events = new Map();

  sleep(ms, signal) {
    return new Promise((resolve, reject) => {
      signal.throwIfAborted();
      const time = this.now + ms;
      const events = this.#events.get(time) ?? new Set();
      this.#events.set(time, events);
      const abort = () => {
        events.delete(wake);
        if (!events.size) this.#events.delete(time);
        reject(signal.reason);
      };
      const wake = () => {
        signal.removeEventListener('abort', abort);
        resolve();
      };
      events.add(wake);
      signal.addEventListener('abort', abort, { once: true });
    });
  }

  async drain() {
    await new Promise(setImmediate);
    while (this.#events.size) {
      this.now = Math.min(...this.#events.keys());
      const events = this.#events.get(this.now);
      this.#events.delete(this.now);
      for (const wake of events) wake();
      await new Promise(setImmediate);
    }
  }
}
