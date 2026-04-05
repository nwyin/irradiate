from timeout_lib import count_up_to, add


def test_count_up_to():
    assert count_up_to(5) == [0, 1, 2, 3, 4]
    assert count_up_to(0) == []


def test_add():
    assert add(1, 2) == 3
