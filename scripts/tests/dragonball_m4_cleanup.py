"""Cleanup for serialized disposable M4 labs; never select devices by host glob."""
import json
from pathlib import Path
import re

from dragonball_m4_lab import command


def owned_device_ids(log):
    plain = re.sub(r"\x1b\[[0-9;]*m", "", log)
    return {int(value) for value in re.findall(
        r"device has been added to ublkdrv[^\n]*?\bdev_id=(\d+)", plain)}


def cleanup_owned_ublk(node, inputs, *, receipt_path=None):
    """After node.close(), attempt all logged allocations and retain the receipt."""
    binary = Path(inputs["ublk_cli"])
    if not binary.is_absolute():
        raise ValueError("ublk_cli must be absolute")
    if node.cgroup.exists() and "populated 1" in (node.cgroup / "cgroup.events").read_text():
        raise RuntimeError("refusing ublk cleanup while node cgroup remains populated")
    log = node.root / "home/logs/ublk-daemon.log"
    receipt, errors = [], []
    if log.exists():
        for dev_id in sorted(owned_device_ids(log.read_text())):
            device = Path(f"/dev/ublkc{dev_id}")
            if not device.exists():
                continue
            item = {"dev_id": dev_id}
            try:
                wrapper = Path(__file__).resolve().parents[1] / "run-with-capabilities.sh"
                result = command(["env", "AENV_RUN_USER=" + node.lab.run_user, "bash", str(wrapper),
                                  str(binary), "delete", str(dev_id)])
                item.update(exit_code=result.returncode, stdout=result.stdout, stderr=result.stderr)
                if device.exists():
                    raise RuntimeError(f"ublk {dev_id} remained")
            except Exception as error:
                item.update(exit_code=getattr(error, "returncode", -1),
                            stdout=getattr(error, "stdout", ""), stderr=str(error))
                errors.append(f"ublk {dev_id}: {error}")
            receipt.append(item)
    try:
        destination = receipt_path if receipt_path is not None else node.lab.root / "ublk-cleanup.json"
        destination.write_text(json.dumps(receipt, indent=2))
    except Exception as error:
        errors.append(f"ublk cleanup receipt: {error}")
    if errors:
        raise RuntimeError("; ".join(errors))
    return receipt
