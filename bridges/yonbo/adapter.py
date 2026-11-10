"""Wire the recovered S3 brain into an owner-installed yonbo_adapter HTTP host."""

import argparse
import asyncio
import os

from .brain_zeroclaw import BridgeError, ZeroClawBridge, read_local_file, read_secret


def parser():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("--brain", choices=("echo", "fixture", "zeroclaw", "spark"), default="echo")
    cli.add_argument("--host", default="127.0.0.1")
    cli.add_argument("--port", type=int, default=8765)
    cli.add_argument("--gateway-endpoint", default=os.environ.get("YONBO_GATEWAY_ENDPOINT", "ws://127.0.0.1:42617/ws/chat"))
    cli.add_argument("--gateway-agent", default=os.environ.get("YONBO_GATEWAY_AGENT"))
    cli.add_argument("--bridge-credential", help="dedicated owner-local mode-0600 token file outside repositories")
    cli.add_argument("--spark-endpoint", default=os.environ.get("YONBO_SPARK_ENDPOINT", "http://127.0.0.1:8000"))
    cli.add_argument("--spark-secrets", help="owner-local mode-0600 plain-text Spark key file")
    cli.add_argument("--secrets", help="owner-local mode-0600 vendor JSON file; omitted uses host synthetic credentials")
    cli.add_argument("--tts", choices=("mock", "say"), default="mock")
    cli.add_argument("--wire-profile", choices=("normal", "capture"), default="normal")
    cli.add_argument("--time-scale", type=float, default=0.0)
    return cli


async def main(args):
    # The vendor HTTP/TTS host is a separate recovery artifact, not a pip dependency.
    try:
        from yonbo_adapter.auth import load_devices
        from yonbo_adapter.server import Adapter
    except ImportError:
        raise BridgeError("Owner-installed yonbo_adapter HTTP/TTS host is required; see README") from None

    if args.secrets:
        read_local_file(args.secrets)
    devices = load_devices(args.secrets) if args.secrets else None
    brain = args.brain
    if brain == "zeroclaw":
        if not args.bridge_credential or not args.gateway_agent:
            raise BridgeError("zeroclaw requires --bridge-credential and --gateway-agent")
        brain = ZeroClawBridge(args.gateway_endpoint, args.gateway_agent, args.bridge_credential)
    elif brain == "spark":
        if not args.spark_secrets:
            raise BridgeError("spark requires --spark-secrets")
        from yonbo_adapter.brain_spark import SparkBrain
        brain = SparkBrain(args.spark_endpoint, read_secret(args.spark_secrets))
    app = Adapter(devices, brain=brain, tts=args.tts, wire_profile=args.wire_profile,
                  time_scale=args.time_scale)
    try:
        await app.start(args.port, host=args.host)
        print("Yonbo adapter listening; content and credentials are not logged", flush=True)
        await asyncio.Event().wait()
    finally:
        await app.close()


if __name__ == "__main__":
    cli = parser()
    try:
        asyncio.run(main(cli.parse_args()))
    except KeyboardInterrupt:
        pass
    except BridgeError as error:
        cli.exit(1, f"{error}\n")
    except Exception:
        cli.exit(1, "Adapter failed; check local configuration (details suppressed).\n")
