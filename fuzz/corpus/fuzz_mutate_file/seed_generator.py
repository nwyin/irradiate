def gen_positives(n: int):
    for i in range(n):
        if i > 0:
            yield i


async def async_gen(n: int):
    for i in range(n):
        if i > 0:
            yield i
