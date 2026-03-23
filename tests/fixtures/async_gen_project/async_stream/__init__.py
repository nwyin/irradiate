class ByteStream:
    """Minimal async stream that yields chunks — mirrors httpx._content.ByteStream."""

    def __init__(self, content: bytes):
        self._content = content

    async def __aiter__(self):
        yield self._content

    async def read(self):
        chunks = []
        async for chunk in self:
            chunks.append(chunk)
        return b"".join(chunks)


class NumberStream:
    """Async stream that yields numbers up to n."""

    def __init__(self, n: int):
        self._n = n

    async def __aiter__(self):
        for i in range(self._n):
            yield i + 1

    async def __anext__(self):
        # Standalone __anext__ (unusual but tests the skip)
        return self._n + 1


def make_stream(data: bytes) -> ByteStream:
    return ByteStream(data)
