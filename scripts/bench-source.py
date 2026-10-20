#!/usr/bin/env python3
"""A bench source-host for the Evo audio plane.

Dials a receiver, says hello (offering bin1), streams 20 ms frames of
PCM s16 48 kHz stereo as bin1 (or as the JSON envelope with --json),
answers its sync probes, and says goodbye. Not Evo itself: a stand-in
for the fan-out, to see a receiver play.

  bench-source.py <host> [--seconds 30] [--tone 440] [--json] [--group g1]
"""
import argparse
import base64
import json
import math
import socket
import struct
import sys
import time

RATE = 48_000
CHANNELS = 2
FRAME_MS = 20
FRAMES = RATE * FRAME_MS // 1000  # 960
MAGIC = 0xEB


def body(sock: socket.socket, b: bytes) -> None:
    sock.sendall(struct.pack(">I", len(b)) + b)


def message(sock: socket.socket, m: dict) -> None:
    body(sock, json.dumps(m, separators=(",", ":")).encode())


def bin1(group: str, seq: int, pts: int, pcm: bytes) -> bytes:
    g, c = group.encode(), b"pcm_s16_le"
    head = struct.pack(">BBBBQQIHI", MAGIC, 1, len(g), len(c), seq, pts, RATE, CHANNELS, len(pcm))
    return head + g + c + pcm


def read_body(sock: socket.socket) -> bytes | None:
    hdr = sock.recv(4, socket.MSG_WAITALL)
    if len(hdr) < 4:
        return None
    (n,) = struct.unpack(">I", hdr)
    out = b""
    while len(out) < n:
        chunk = sock.recv(n - len(out))
        if not chunk:
            return None
        out += chunk
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("--port", type=int, default=7331)
    ap.add_argument("--seconds", type=float, default=30)
    ap.add_argument("--tone", type=float, default=440)
    ap.add_argument("--json", action="store_true", help="send the JSON envelope, as an older source-host")
    ap.add_argument("--group", default="bench-group")
    ap.add_argument("--id", default="00000000-0000-4000-8000-00000000bench")
    a = ap.parse_args()

    s = socket.create_connection((a.host, a.port), timeout=5)
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    message(s, {
        "type": "hello",
        "device_id": a.id,
        "framework_version": "bench",
        "claimed_source_host_groups": [a.group],
        "formats": ["bin1"],
    })
    their = read_body(s)
    print("their hello:", their.decode(errors="replace") if their else None)
    s.settimeout(0.0)

    # Frames at 50 Hz, presentation time = ms since start, as the Linux
    # source stamps them.
    phase = 0.0
    step = 2 * math.pi * a.tone / RATE
    start = time.monotonic()
    seq = 0
    sent = 0
    probes = 0
    while time.monotonic() - start < a.seconds:
        due = start + seq * FRAME_MS / 1000
        now = time.monotonic()
        if now < due:
            time.sleep(due - now)
        samples = bytearray()
        for _ in range(FRAMES):
            v = int(8000 * math.sin(phase))
            phase += step
            samples += struct.pack("<hh", v, v)
        pts = int((time.monotonic() - start) * 1000)
        if a.json:
            message(s, {
                "type": "audio_frame", "group_id": a.group, "sequence": seq,
                "presentation_time_ms": pts, "codec": "pcm_s16_le",
                "rate_hz": RATE, "channels": CHANNELS,
                "payload_b64": base64.b64encode(bytes(samples)).decode(),
            })
        else:
            body(s, bin1(a.group, seq, pts, bytes(samples)))
        seq += 1
        sent += len(samples)
        # Anything from the receiver: answer probes.
        try:
            b = read_body(s)
        except (BlockingIOError, socket.timeout):
            b = b""
        if b:
            try:
                m = json.loads(b)
            except Exception:
                m = {}
            if m.get("type") == "sync_probe":
                probes += 1
                message(s, {"type": "sync_response", "group_id": m["group_id"], "t1_ms": m["t1_ms"], "t2_ms": pts})
                print(f"probe {probes} answered at {pts} ms")
            elif m:
                print("message:", m)
    message(s, {"type": "goodbye", "device_id": a.id})
    print(f"sent {seq} frames, {sent} bytes, {probes} probes answered; goodbye")
    s.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
