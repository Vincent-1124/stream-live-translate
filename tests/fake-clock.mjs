// Deterministic virtual clock for the overlay regression tests.
//
// The overlay leans on setTimeout/setInterval for three things the tests must
// pin down: the first-render display buffer, the clear-after-silence timer and
// the typewriter animation.  Waiting on the real clock would make the suite
// slow and flaky, so every timer is registered here and only fires when a test
// advances the timeline.

export class FakeClock {
  constructor({ startTime = 1700000000000, maxStepsPerTick = 200000 } = {}) {
    this.now = startTime;
    this.maxStepsPerTick = maxStepsPerTick;
    this._seq = 0;
    this._timers = new Map(); // id -> { id, at, fn, kind, interval }
    this._order = []; // ids, kept unsorted; we scan for the earliest
    this.steps = 0;
  }

  get pendingCount() { return this._timers.size; }

  /** Every live timer, sorted by deadline (test introspection / assertions). */
  pending() {
    return [...this._timers.values()].sort((a, b) => a.at - b.at || a.id - b.id);
  }

  setTimeout(fn, delay, ...args) {
    const id = ++this._seq;
    const ms = Number(delay);
    this._timers.set(id, {
      id,
      at: this.now + (isFinite(ms) && ms > 0 ? ms : 0),
      fn,
      args,
      kind: "timeout",
      interval: null,
    });
    this._order.push(id);
    return id;
  }

  clearTimeout(id) {
    if (id === null || id === undefined) return;
    this._timers.delete(id);
  }

  setInterval(fn, delay, ...args) {
    const id = ++this._seq;
    const ms = Number(delay);
    const period = isFinite(ms) && ms > 0 ? ms : 1;
    this._timers.set(id, {
      id,
      at: this.now + period,
      fn,
      args,
      kind: "interval",
      interval: period,
    });
    this._order.push(id);
    return id;
  }

  clearInterval(id) { this.clearTimeout(id); }

  _earliest() {
    let best = null;
    for (const t of this._timers.values()) {
      if (!best || t.at < best.at || (t.at === best.at && t.id < best.id)) best = t;
    }
    return best;
  }

  /**
   * Advance the virtual timeline by `ms`, firing due timers in deadline order
   * and letting each callback schedule new timers at the current virtual time.
   */
  tick(ms = 0) {
    const target = this.now + (Number(ms) || 0);
    let steps = 0;
    for (;;) {
      const next = this._earliest();
      if (!next || next.at > target) break;
      if (++steps > this.maxStepsPerTick) {
        throw new Error(
          `fake clock: more than ${this.maxStepsPerTick} timer callbacks while advancing ${ms} ms ` +
            `(runaway timer loop?)`
        );
      }
      this.steps++;
      this.now = next.at;
      if (next.kind === "interval") {
        next.at = this.now + next.interval;
      } else {
        this._timers.delete(next.id);
      }
      next.fn(...next.args);
    }
    this.now = target;
    return this;
  }
}

export default FakeClock;
