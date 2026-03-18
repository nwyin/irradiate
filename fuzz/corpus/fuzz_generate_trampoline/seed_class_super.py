class Base:
    def compute(self, x: int) -> int:
        return x + 1


class Child(Base):
    def compute(self, x: int) -> int:
        base = super().compute(x)
        return base * 2 if x > 0 else base
