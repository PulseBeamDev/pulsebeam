export class Track extends EventTarget {
  id = crypto.randomUUID();
  enabled = true;
  readyState = "live";
  constructor(kind = "video") {
    super();
    this.kind = kind;
  }
  clone() {
    return new Track(this.kind);
  }
  stop() {
    this.readyState = "ended";
  }
}
export class Stream {
  constructor(tracks = []) {
    this.tracks = tracks;
  }
  getTracks() {
    return this.tracks;
  }
}
Object.assign(globalThis, { MediaStreamTrack: Track, MediaStream: Stream });
