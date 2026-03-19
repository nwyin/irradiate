"""Tests for harness/worker.py helper logic and plugin-owned run state."""

import json
import socket
from unittest.mock import MagicMock

# conftest.py inserts the repo root into sys.path so these imports resolve
from harness.worker import MutationWorkerPlugin, recv_message, reports_indicate_failure, send_message


# ---------------------------------------------------------------------------
# send_message
# ---------------------------------------------------------------------------


def test_send_message_format():
    """send_message must write JSON + newline, UTF-8 encoded."""
    reader, writer = socket.socketpair()
    try:
        send_message(writer, {"type": "test", "value": 42})
        data = reader.recv(4096)
    finally:
        reader.close()
        writer.close()

    assert data.endswith(b"\n"), "message must end with newline"
    parsed = json.loads(data.decode("utf-8"))
    assert parsed == {"type": "test", "value": 42}


def test_send_message_unicode():
    """send_message must correctly encode unicode payloads."""
    reader, writer = socket.socketpair()
    try:
        send_message(writer, {"key": "xǁClassǁfoo"})
        data = reader.recv(4096)
    finally:
        reader.close()
        writer.close()

    parsed = json.loads(data.decode("utf-8"))
    assert parsed["key"] == "xǁClassǁfoo"


# ---------------------------------------------------------------------------
# recv_message
# ---------------------------------------------------------------------------


def test_recv_message_complete():
    """recv_message parses a complete JSON line from the socket."""
    reader, writer = socket.socketpair()
    try:
        writer.sendall(b'{"type":"ack","ok":true}\n')
        writer.close()
        msg, remaining = recv_message(reader, bytearray())
    finally:
        reader.close()

    assert msg == {"type": "ack", "ok": True}
    assert remaining == b""


def test_recv_message_accumulates_partial_chunks():
    """recv_message reassembles a message split across multiple socket reads."""
    sock = MagicMock()
    # Simulate two partial reads before the newline arrives
    sock.recv.side_effect = [b'{"type":', b'"run","n":1}\n']

    msg, buf = recv_message(sock, bytearray())

    assert msg == {"type": "run", "n": 1}
    assert buf == b""


def test_recv_message_leaves_remainder_in_buf():
    """recv_message returns the remaining bytes after the first newline."""
    sock = MagicMock()
    sock.recv.side_effect = [b'{"a":1}\n{"b":2}\n']

    msg, buf = recv_message(sock, bytearray())

    assert msg == {"a": 1}
    # Second message sits in the returned buf, ready for the next call
    assert buf == b'{"b":2}\n'


def test_recv_message_buf_already_contains_complete_message():
    """recv_message must not call recv if the buf already contains a full line."""
    sock = MagicMock()
    prefilled_buf = bytearray(b'{"type":"ready"}\n')

    msg, remaining = recv_message(sock, prefilled_buf)

    assert msg == {"type": "ready"}
    assert remaining == b""
    sock.recv.assert_not_called()


def test_recv_message_connection_closed_empty_buf():
    """recv_message returns (None, buf) when the connection closes mid-read."""
    sock = MagicMock()
    sock.recv.return_value = b""  # connection closed

    msg, _ = recv_message(sock, bytearray())

    assert msg is None


def test_recv_message_connection_closed_partial_buf():
    """recv_message preserves partial buffer when connection closes without newline."""
    sock = MagicMock()
    sock.recv.return_value = b""  # connection closed
    partial = bytearray(b'{"type":"inc')

    msg, buf = recv_message(sock, partial)

    assert msg is None
    # Partial buffer must be preserved so the caller can surface the error
    assert b'{"type":"inc' in bytes(buf)


# ---------------------------------------------------------------------------
# report classification
# ---------------------------------------------------------------------------


def test_reports_indicate_failure_false_when_all_reports_pass():
    report_a = MagicMock(failed=False)
    report_b = MagicMock(failed=False)

    assert reports_indicate_failure([report_a, report_b]) is False


def test_reports_indicate_failure_true_when_any_report_fails():
    report_a = MagicMock(failed=False)
    report_b = MagicMock(failed=True)

    assert reports_indicate_failure([report_a, report_b]) is True


def test_reports_indicate_failure_empty_list():
    assert reports_indicate_failure([]) is False


# ---------------------------------------------------------------------------
# MutationWorkerPlugin helper state
# ---------------------------------------------------------------------------


def make_plugin():
    return MutationWorkerPlugin(sock=MagicMock(), use_legacy=False)


def make_item(nodeid):
    item = MagicMock()
    item.nodeid = nodeid
    return item


def test_prepare_items_sorts_by_collection_order():
    plugin = make_plugin()
    plugin.items = {
        "tests/test_mod.py::test_b": make_item("tests/test_mod.py::test_b"),
        "tests/test_mod.py::test_a": make_item("tests/test_mod.py::test_a"),
    }
    plugin.item_order = {
        "tests/test_mod.py::test_a": 0,
        "tests/test_mod.py::test_b": 1,
    }

    items = plugin._prepare_items(["tests/test_mod.py::test_b", "tests/test_mod.py::test_a"])

    assert [item.nodeid for item in items] == [
        "tests/test_mod.py::test_a",
        "tests/test_mod.py::test_b",
    ]


def test_prepare_items_skips_unknown_nodeids():
    plugin = make_plugin()
    plugin.items = {"tests/test_mod.py::test_a": make_item("tests/test_mod.py::test_a")}
    plugin.item_order = {"tests/test_mod.py::test_a": 0}

    items = plugin._prepare_items(["tests/test_mod.py::test_missing", "tests/test_mod.py::test_a"])

    assert [item.nodeid for item in items] == ["tests/test_mod.py::test_a"]


def test_pytest_runtest_logreport_records_only_active_item():
    plugin = make_plugin()
    plugin.current_run_mutant = "mod.x_func__irradiate_1"
    plugin.current_item_nodeid = "tests/test_mod.py::test_a"

    report_a = MagicMock(nodeid="tests/test_mod.py::test_a")
    report_b = MagicMock(nodeid="tests/test_mod.py::test_b")

    plugin.pytest_runtest_logreport(report_a)
    plugin.pytest_runtest_logreport(report_b)

    assert plugin.current_run_reports == [report_a]


def test_reset_run_state_clears_plugin_fields():
    plugin = make_plugin()
    plugin.current_run_mutant = "mod.x_func__irradiate_1"
    plugin.current_run_nodeids = {"tests/test_mod.py::test_a"}
    plugin.current_item_nodeid = "tests/test_mod.py::test_a"
    plugin.current_run_reports = [MagicMock()]

    plugin._reset_run_state()

    assert plugin.current_run_mutant is None
    assert plugin.current_run_nodeids == set()
    assert plugin.current_item_nodeid is None
    assert plugin.current_run_reports == []
