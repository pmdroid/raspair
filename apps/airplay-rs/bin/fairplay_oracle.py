#!/usr/bin/env python3
"""FairPlay Oracle — standalone fake AirPlay receiver.

Advertises as an AirPlay device on Bonjour. When the Mac connects and does
`/fp-setup`, the configured reply is returned and the Mac's setup2 is captured.

Usage:
  python3 fairplay_oracle.py [--ctrl-port 9999]

Control socket (127.0.0.1:CTRL_PORT, line-based):
  → INJECT_REPLY <hex-142-bytes>
  ← OK reply armed

  → WAITING
  ← SETUP2 <hex>   (blocks until Mac sends setup2)

  → QUIT
  → PING
"""

import argparse
import asyncio
import binascii
import ipaddress
import logging
import plistlib
import sys
from typing import Optional

from zeroconf import Zeroconf
from zeroconf._exceptions import NonUniqueNameException

from pyatv.core import mdns
from pyatv.support import net, http
from pyatv.protocols.airplay.server_auth import (
    AirPlayServerAuth,
    PlayFair,
    SERVER_IDENTIFIER,
)
from pyatv.auth.hap_session import HAPSession

_LOGGER = logging.getLogger("fairplay_oracle")
FAKE_NAME = "FairPlay Oracle"


class OraclePlayFair(PlayFair):
    """PlayFair variant that returns an injected reply and captures setup2."""

    def __init__(self) -> None:
        super().__init__()
        self._injected_reply: Optional[bytes] = None
        self._captured_setup2: Optional[bytes] = None
        self._setup2_event = asyncio.Event()

    def inject_reply(self, reply: bytes) -> None:
        self._injected_reply = reply
        self._captured_setup2 = None
        self._setup2_event.clear()
        _LOGGER.info("Injected reply (%d bytes) armed", len(reply))

    async def wait_for_setup2(self) -> bytes:
        await self._setup2_event.wait()
        assert self._captured_setup2 is not None
        return self._captured_setup2

    def fairplay_setup(self, request: bytes):
        if len(request) < 15 or request[4] != 3:
            return -1
        msg_type = request[self.TYPE_POSITION]
        seq = request[self.SEQ_POSITION]

        if msg_type == self.SETUP_MESSAGE_TYPE:
            if seq == self.SETUP1_MESSAGE_SEQ:
                if self._injected_reply is not None:
                    _LOGGER.info(
                        "Returning injected reply (%d bytes)", len(self._injected_reply)
                    )
                    return self._injected_reply
                mode = request[self.MODE_POSITON]
                _LOGGER.warning("No injected reply — falling back to canned mode %d", mode)
                return self.reply_message[mode]
            elif seq == self.SETUP2_MESSAGE_SEQ:
                self._captured_setup2 = bytes(request)
                _LOGGER.info(
                    "Captured setup2 (%d bytes): %s",
                    len(self._captured_setup2),
                    self._captured_setup2.hex(),
                )
                self._setup2_event.set()
                return self.fp_header + request[
                    len(request) - self.SETUP2_RESPONSE_SUFFIX_LENGTH:
                ]
        return None


class EncryptedHttpServer(http.BasicHttpServer):
    """BasicHttpServer that transparently encrypts/decrypts with HAP session."""

    def __init__(self, handler):
        super().__init__(handler)
        self._hap_session = None
        self._encrypt_responses = False

    def _init_session(self, handler):
        if hasattr(handler, '_encryption_keys'):
            ok, ik = handler._encryption_keys
            if ok is not None:
                self._hap_session = HAPSession()
                self._hap_session.enable(ok, ik)
                return True
        return False

    def process_received(self, data):
        if self._hap_session is None:
            self._init_session(self.handler)
        if self._hap_session is not None:
            if not self._encrypt_responses:
                self._encrypt_responses = True
            data = self._hap_session.decrypt(data)
        return data

    def process_sent(self, data):
        if self._encrypt_responses and self._hap_session:
            data = self._hap_session.encrypt(data)
        return data


class OracleAirPlayServer(AirPlayServerAuth):
    """Fake AirPlay receiver with oracle FairPlay and a control socket."""

    def __init__(self, ctrl_port: int = 9999):
        super().__init__(FAKE_NAME)
        self._ctrl_port = ctrl_port
        self._zconf: Optional[Zeroconf] = None
        self._unpublisher = None
        self._oracle = OraclePlayFair()
        self._encryption_keys = [None, None]  # [output_key, input_key]
        self._airplay_port: int = 0
        self._mac_addr: str = ""
        self._bonjour_pending: dict = {}  # kwargs for publish_bonjour()
        self._captured_shk: Optional[str] = None
        self.add_route("POST", "^/session/.*/setup$", self.handle_setup_capture)

    def handle_setup_capture(self, request):
        _LOGGER.info("Mac sent SETUP! body=%d bytes", len(request.body))
        try:
            parsed = plistlib.loads(request.body)
            shk = parsed.get("shk") or parsed.get("streams", [{}])[0].get("shk")
            if shk:
                self._captured_shk = shk.hex() if isinstance(shk, bytes) else str(shk)
                _LOGGER.info("CAPTURED shk: %s", self._captured_shk)
            _LOGGER.info("Full SETUP body: %s", parsed)
        except Exception as e:
            _LOGGER.warning("Could not parse SETUP body: %s", e)
        return http.HttpResponse(
            request.protocol, request.version, 200, "OK",
            {"CSeq": request.headers.get("CSeq", "0")},
            b"",
        )

    def enable_encryption(self, output_key: bytes, input_key: bytes) -> None:
        self._encryption_keys[0] = output_key
        self._encryption_keys[1] = input_key

    def handle_fp_setup(self, request):
        response = self._oracle.fairplay_setup(request.body)
        return http.HttpResponse(
            request.protocol, request.version,
            200, "OK",
            {"CSeq": request.headers.get("CSeq", "0")},
            response if isinstance(response, bytes) else b"",
        )

    def handle_info(self, request: http.HttpRequest):
        _LOGGER.info("Mac sent /info probe")
        body = {
            "psi": SERVER_IDENTIFIER,
            "vv": 2,
            "playbackCapabilities": {
                "supportsInterstitials": False,
                "supportsFPSSecureStop": False,
                "supportsUIForAudioOnlyContent": False,
            },
            "canRecordScreenStream": False,
            "statusFlags": 4,
            "keepAliveSendStatsAsBody": True,
            "protocolVersion": "1.1",
            "volumeControlType": 3,
            "name": FAKE_NAME,
            "senderAddress": "10.0.10.254:45285",
            "pi": "a1023589-a2ef-47de-8596-42f6fe5caacd",
            "screenDemoMode": False,
            "initialVolume": -27.0,
            "featuresEx": "AMp/StBLNTwQoa54",
            "supportedFormats": {
                "lowLatencyAudioStream": 4398080065536,
                "screenStream": 21235712,
                "audioStream": 21235712,
                "bufferStream": 1649282121728,
            },
            "sourceVersion": "950.7.1",
            "model": "AudioAccessory6,1",
            "pk": self.keys.auth_pub,
            "receiverHDRCapability": "4k60",
            "macAddress": self._mac_addr,
            "deviceID": self._mac_addr,
            "features": "0x4A7FCA00,0x3C354BD0",
        }
        return http.HttpResponse(
            "RTSP", "1.0", 200, "OK",
            {"CSeq": request.headers.get("CSeq", "0")},
            plistlib.dumps(body),
        )

    async def publish_bonjour(self) -> None:
        loop = self._bonjour_pending["loop"]
        local_ip = self._bonjour_pending["local_ip"]
        actual_port = self._bonjour_pending["actual_port"]
        mac = self._bonjour_pending["mac"]
        if self._unpublisher is not None:
            _LOGGER.info("Bonjour already published, skipping")
            return
        props = {
            "acl": "0",
            "pw": "false",
            "srcvers": "950.7.1",
            "features": "0x4A7FCA00,0x3C354BD0",
            "fex": "AMp/StBLNTwQoa54",
            "flags": "0xb8c04",
            "model": "AudioAccessory6,1",
            "psi": SERVER_IDENTIFIER,
            "gid": SERVER_IDENTIFIER,
            "pi": "a1023589-a2ef-47de-8596-42f6fe5caacd",
            "pk": binascii.hexlify(self.keys.auth_pub).decode(),
            "deviceid": mac,
            "protovers": "1.1",
            "osvers": "26.5",
            "vv": "1",
        }
        name = FAKE_NAME
        for attempt in range(10):
            try:
                self._unpublisher = await mdns.publish(
                    loop,
                    mdns.Service(
                        "_airplay._tcp.local",
                        name,
                        ipaddress.IPv4Address(local_ip),
                        actual_port,
                        props,
                    ),
                    self._zconf,
                )
                break
            except NonUniqueNameException:
                _LOGGER.warning("Name '%s' taken, trying suffix _%d", name, attempt)
                name = f"{FAKE_NAME}_{attempt}"
                await asyncio.sleep(1)
        else:
            raise RuntimeError("Could not publish Bonjour service after 10 attempts")
        _LOGGER.info("Published '%s' at %s:%d", name, local_ip, actual_port)

    async def _handle_ctrl(self, reader, writer):
        peer = writer.get_extra_info("peername")
        _LOGGER.info("Control client: %s", peer)
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                parts = line.decode("ascii").strip().split(maxsplit=1)
                cmd = parts[0].upper()
                arg = parts[1] if len(parts) > 1 else ""

                if cmd == "INJECT_REPLY":
                    reply = binascii.unhexlify(arg)
                    if len(reply) != 142:
                        writer.write(b"ERROR expected 142 bytes\n")
                    else:
                        self._oracle.inject_reply(reply)
                        await self.publish_bonjour()
                        writer.write(b"OK reply armed\n")

                elif cmd == "WAITING":
                    _LOGGER.info("Waiting for Mac to send setup2...")
                    setup2 = await self._oracle.wait_for_setup2()
                    writer.write(f"SETUP2 {setup2.hex()}\n".encode())

                elif cmd == "QUIT":
                    writer.write(b"OK\n")
                    await writer.drain()
                    asyncio.get_event_loop().stop()
                    break

                elif cmd == "BONJOUR":
                    _LOGGER.info("Publishing Bonjour service...")
                    await self.publish_bonjour()
                    writer.write(b"OK Bonjour published\n")

                elif cmd == "PING":
                    writer.write(b"PONG\n")

                else:
                    writer.write(b"ERROR unknown\n")

                await writer.drain()
        except Exception as e:
            _LOGGER.error("Ctrl: %s", e)
        finally:
            writer.close()

    async def start(self, local_ip: Optional[str] = None) -> None:
        loop = asyncio.get_event_loop()
        if local_ip is None:
            private = net.get_private_addresses(include_loopback=False)
            local_ip = str(private[0]) if private else "127.0.0.1"

        def factory():
            return EncryptedHttpServer(self)

        server = await loop.create_server(factory, "0.0.0.0", 0)
        actual_port = server.sockets[0].getsockname()[1]
        _LOGGER.info("AirPlay receiver on port %d", actual_port)

        self._zconf = Zeroconf(interfaces=[local_ip])
        raw = SERVER_IDENTIFIER.replace("-", "")
        mac = ":".join(raw[i:i+2].upper() for i in range(0, 12, 2))
        self._mac_addr = mac
        # Save params for deferred Bonjour publish (triggered via BONJOUR cmd)
        self._bonjour_pending = dict(
            loop=loop,
            local_ip=local_ip,
            actual_port=actual_port,
            mac=mac,
        )
        _LOGGER.info("HTTP ready on %s:%d (Bonjour deferred until BONJOUR command)", local_ip, actual_port)

        ctrl = await asyncio.start_server(
            self._handle_ctrl, "127.0.0.1", self._ctrl_port
        )
        _LOGGER.info("Control socket on 127.0.0.1:%d", self._ctrl_port)

        print(f"""
FairPlay Oracle (standalone receiver)
  Bonjour: {FAKE_NAME} ({local_ip}:{actual_port})
  Control: 127.0.0.1:{self._ctrl_port}

  1. AirPlay audio from your Mac to "{FAKE_NAME}"
  2. Use control socket to inject/setup2
""")

    async def stop(self) -> None:
        if self._unpublisher:
            await self._unpublisher()
        if self._zconf:
            self._zconf.close()


async def main():
    parser = argparse.ArgumentParser(description="FairPlay Oracle")
    parser.add_argument("--ctrl-port", type=int, default=9999)
    parser.add_argument("--local-ip", default=None)
    parser.add_argument("--verbose", "-v", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        stream=sys.stdout,
        format="%(asctime)s %(levelname)s [%(name)s]: %(message)s",
        datefmt="%H:%M:%S",
        force=True,
    )

    server = OracleAirPlayServer(ctrl_port=args.ctrl_port)
    await server.start(local_ip=args.local_ip)
    try:
        while True:
            await asyncio.sleep(3600)
    except KeyboardInterrupt:
        pass
    finally:
        await server.stop()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
