def count_up_to(n):
    """Count from 0 to n-1. Mutating the `<` to `>` or `<=` creates an infinite loop."""
    i = 0
    result = []
    while i < n:
        result.append(i)
        i += 1
    return result


def add(a, b):
    return a + b
