from dataclasses import dataclass


@dataclass
class Point:
    x: float
    y: float


def distance(p1: Point, p2: Point) -> float:
    dx: float = p1.x - p2.x
    dy: float = p1.y - p2.y
    return (dx**2 + dy**2) ** 0.5


def clamp(value: int, low: int, high: int) -> int:
    if value < low:
        return low
    if value > high:
        return high
    return value


def repeat_string(s: str, n: int) -> str:
    return s * n


def invert_mapping(d: dict[str, int]) -> dict[int, str]:
    result: dict[int, str] = {}
    for k, v in d.items():
        result[v] = k
    return result


def safe_divide(a: float, b: float) -> float | None:
    if b == 0:
        return None
    return a / b


def count_positives(numbers: list[int]) -> int:
    count: int = 0
    for n in numbers:
        if n > 0:
            count += 1
    return count


def format_pair(key: str, value: int) -> str:
    return key + "=" + str(value)
