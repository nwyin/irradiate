"""
Tests for harness/worker.py — send_message, recv_message, reset_run_state.

The IPC loop, main(), _force_teardown(), and MutationWorkerPlugin are not
unit-testable without a full pytest session; they are excluded by design.
"""

import json
import socket
from unittest.mock import MagicMock

# conftest.py inserts the repo root into sys.path so these imports resolve
from harness.worker import recv_message, reset_run_state, send_message


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
# reset_run_state
# ---------------------------------------------------------------------------


def test_reset_run_state_clears_report_sections():
    """reset_run_state clears _report_sections on each item (INV-2)."""
    item1 = MagicMock()
    item1._report_sections = [("stdout", "some output"), ("stderr", "error")]
    item2 = MagicMock()
    item2._report_sections = [("stdout", "more output")]

    reset_run_state([item1, item2])

    assert item1._report_sections == []
    assert item2._report_sections == []


def test_reset_run_state_tolerates_missing_report_sections():
    """reset_run_state must not crash if an item lacks _report_sections."""
    item = MagicMock(spec=[])  # no attributes at all
    reset_run_state([item])  # must not raise


def test_reset_run_state_empty_list():
    """reset_run_state is a no-op on an empty item list."""
    reset_run_state([])  # must not raise


def test_reset_run_state_already_empty():
    """reset_run_state on items with empty _report_sections is idempotent."""
    item = MagicMock()
    item._report_sections = []
    reset_run_state([item])
    assert item._report_sections == []
