// Geometry, observer notifications and media completion are external inputs.
// These doubles do not claim to compute layout or decode media.
export class Stream {
  constructor(tracks = []) {
    this.tracks = new Set(tracks);
  }
  getAudioTracks() {
    return this.getTracks().filter((track) => track.kind === "audio");
  }
  getTracks() {
    return [...this.tracks];
  }
  addTrack(track) {
    this.tracks.add(track);
  }
  removeTrack(track) {
    this.tracks.delete(track);
  }
}
export const queries = [];
export const viewport = Object.assign(new EventTarget(), {
  innerWidth: 800,
  innerHeight: 600,
  devicePixelRatio: 2,
  getComputedStyle: (node) => node.style,
  matchMedia() {
    const query = new EventTarget();
    queries.push(query);
    return query;
  },
});
export const document = Object.assign(new EventTarget(), {
  hidden: false,
  defaultView: viewport,
});
export class Video {
  isConnected = true;
  srcObject = null;
  parentElement = null;
  ownerDocument = document;
  style = { display: "block", visibility: "visible", opacity: "1" };
  rect = { top: 0, left: 0, width: 160, height: 120 };
  getBoundingClientRect() {
    return {
      ...this.rect,
      bottom: this.rect.top + this.rect.height,
      right: this.rect.left + this.rect.width,
    };
  }
  play() {
    return Promise.resolve();
  }
}
function observer() {
  const active = new Set();
  return class {
    constructor(callback) {
      this.callback = callback;
    }
    observe() {
      active.add(this);
    }
    disconnect() {
      active.delete(this);
    }
    static notify(value) {
      for (const item of active) item.callback(value);
    }
  };
}
export const Resize = observer();
export const Intersection = observer();
export const Mutation = observer();
Object.assign(globalThis, {
  MediaStream: Stream,
  HTMLVideoElement: Video,
  ResizeObserver: Resize,
  IntersectionObserver: Intersection,
  MutationObserver: Mutation,
});
