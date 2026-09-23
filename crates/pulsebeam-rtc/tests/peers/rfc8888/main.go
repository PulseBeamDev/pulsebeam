// This peer uses Pion's DTLS-SRTP transport to authenticate RFC 8888 RTCP.
// Its raw feedback encoder is intentionally independent of PulseBeam's parser.
package main

import (
	"bufio"
	"encoding/base64"
	"encoding/binary"
	"fmt"
	"net"
	"os"
	"strings"
	"time"

	"github.com/pion/rtcp"
	"github.com/pion/webrtc/v4"
	"github.com/pion/webrtc/v4/pkg/media"
)

func fail(err error) {
	fmt.Fprintln(os.Stderr, err)
	os.Exit(1)
}

func feedback(ssrc uint32, sequence uint16, elapsed time.Duration) rtcp.RawPacket {
	packet := make([]byte, 24)
	packet[0] = 0x8b // RTCP version 2, RTPFB format 11 (CCFB).
	packet[1] = 205
	binary.BigEndian.PutUint16(packet[2:4], 5)
	binary.BigEndian.PutUint32(packet[4:8], 0x11223344)
	binary.BigEndian.PutUint32(packet[8:12], ssrc)
	binary.BigEndian.PutUint16(packet[12:14], sequence)
	binary.BigEndian.PutUint16(packet[14:16], 1)
	binary.BigEndian.PutUint16(packet[16:18], 0x8001) // Received, non-ECN, 1/1024 s ago.
	binary.BigEndian.PutUint32(packet[20:24], uint32(elapsed.Microseconds()*65536/1_000_000))
	return rtcp.RawPacket(packet)
}

func main() {
	var engine webrtc.MediaEngine
	if err := engine.RegisterCodec(webrtc.RTPCodecParameters{
		RTPCodecCapability: webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeOpus, ClockRate: 48000, Channels: 2},
		PayloadType:        111,
	}, webrtc.RTPCodecTypeAudio); err != nil {
		fail(err)
	}
	if err := engine.RegisterHeaderExtension(
		webrtc.RTPHeaderExtensionCapability{URI: "urn:ietf:params:rtp-hdrext:sdes:mid"},
		webrtc.RTPCodecTypeAudio,
	); err != nil {
		fail(err)
	}
	var settings webrtc.SettingEngine
	settings.SetIncludeLoopbackCandidate(true)
	settings.SetHandleUndeclaredSSRCWithoutAnswer(true)
	settings.SetIPFilter(func(ip net.IP) bool { return ip.IsLoopback() && ip.To4() != nil })
	api := webrtc.NewAPI(webrtc.WithMediaEngine(&engine), webrtc.WithSettingEngine(settings))
	pc, err := api.NewPeerConnection(webrtc.Configuration{})
	if err != nil {
		fail(err)
	}
	defer pc.Close()
	track, err := webrtc.NewTrackLocalStaticSample(
		webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeOpus, ClockRate: 48000, Channels: 2},
		"source", "rfc8888-peer",
	)
	if err != nil {
		fail(err)
	}
	sender, err := pc.AddTrack(track)
	if err != nil {
		fail(err)
	}
	go func() {
		for {
			if _, _, err := sender.Read(make([]byte, 1500)); err != nil {
				return
			}
		}
	}()
	start := time.Now()
	pc.OnTrack(func(remote *webrtc.TrackRemote, _ *webrtc.RTPReceiver) {
		go func() {
			for {
				packet, _, err := remote.ReadRTP()
				if err != nil {
					return
				}
				raw := feedback(uint32(remote.SSRC()), packet.SequenceNumber, time.Since(start))
				if err := pc.WriteRTCP([]rtcp.Packet{&raw}); err != nil {
					fmt.Fprintln(os.Stderr, "CCFB:", err)
					return
				}
			}
		}()
	})
	offer, err := pc.CreateOffer(nil)
	if err != nil {
		fail(err)
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(offer); err != nil {
		fail(err)
	}
	<-gathered
	// Pion does not advertise CCFB itself. Only the advertised RTCP feedback
	// capability is modified; the authenticated RTP/RTCP peer remains Pion.
	wireOffer := pc.LocalDescription().SDP
	var lines []string
	for _, line := range strings.Split(wireOffer, "\r\n") {
		if (strings.HasPrefix(line, "a=rtcp-fb:") && strings.Contains(line, "transport-cc")) ||
			(strings.HasPrefix(line, "a=extmap:") && strings.Contains(line, "transport-wide-cc")) {
			continue
		}
		lines = append(lines, line)
		if strings.HasPrefix(line, "a=rtpmap:111 ") {
			lines = append(lines, "a=rtcp-fb:111 ccfb")
		}
	}
	fmt.Println(base64.StdEncoding.EncodeToString([]byte(strings.Join(lines, "\r\n"))))
	input := bufio.NewScanner(os.Stdin)
	if !input.Scan() {
		fail(fmt.Errorf("missing answer"))
	}
	answer, err := base64.StdEncoding.DecodeString(input.Text())
	if err != nil {
		fail(err)
	}
	if err := pc.SetRemoteDescription(webrtc.SessionDescription{Type: webrtc.SDPTypeAnswer, SDP: string(answer)}); err != nil {
		fail(err)
	}
	connected := make(chan struct{}, 1)
	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if state == webrtc.PeerConnectionStateConnected {
			select {
			case connected <- struct{}{}:
			default:
			}
		}
	})
	select {
	case <-connected:
	case <-time.After(10 * time.Second):
		fail(fmt.Errorf("peer did not connect"))
	}
	for i := 0; i < 150; i++ {
		if err := track.WriteSample(media.Sample{Data: []byte{0xf8, 0xff, 0xfe}, Duration: 20 * time.Millisecond}); err != nil {
			fail(err)
		}
		time.Sleep(20 * time.Millisecond)
	}
	time.Sleep(time.Second)
}
