// External completion controls only. There is no SDP, RTP, decoding or browser
// compatibility implementation here: negotiation intentionally never completes.
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
class Sender {
  track = null;
  constructor(encodings) {
    this.encodings = encodings;
  }
  async replaceTrack(track) {
    this.track = track;
  }
  getParameters() {
    return { encodings: this.encodings.map((encoding) => ({ ...encoding })) };
  }
  async setParameters(parameters) {
    this.encodings = parameters.encodings;
  }
}
class Transceiver {
  constructor(sender) {
    this.sender = sender;
  }
}
class Peer {
  connectionState = "new";
  addTransceiver(kind, init) {
    return new Transceiver(new Sender(init.sendEncodings ?? [{}]));
  }
  createDataChannel(label) {
    return { label, close() {} };
  }
  createOffer() {
    return new Promise(() => {});
  }
  close() {
    this.connectionState = "closed";
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
Object.assign(globalThis, {
  MediaStream: Stream,
  RTCPeerConnection: Peer,
  RTCRtpTransceiver: Transceiver,
  RTCRtpSender: Sender,
  MediaStreamTrack: Track,
});
