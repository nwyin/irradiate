import asyncio
import gc
import pytest
from async_stream import ByteStream, NumberStream, make_stream


@pytest.mark.asyncio
async def test_byte_stream_read():
    stream = ByteStream(b"hello")
    result = await stream.read()
    assert result == b"hello"


@pytest.mark.asyncio
async def test_byte_stream_iter():
    stream = ByteStream(b"world")
    chunks = []
    async for chunk in stream:
        chunks.append(chunk)
    assert chunks == [b"world"]


@pytest.mark.asyncio
async def test_number_stream():
    stream = NumberStream(3)
    numbers = []
    async for n in stream:
        numbers.append(n)
    assert numbers == [1, 2, 3]


@pytest.mark.asyncio
async def test_byte_stream_not_consumed():
    """Create an async generator but don't consume it.
    When GC'd after event loop closes, this triggers
    PytestUnraisableExceptionWarning if filterwarnings=error."""
    stream = ByteStream(b"unused")
    ait = stream.__aiter__()
    # Don't consume - let it be GC'd
    del ait
    gc.collect()


@pytest.mark.asyncio
async def test_make_stream():
    stream = make_stream(b"test")
    result = await stream.read()
    assert result == b"test"
