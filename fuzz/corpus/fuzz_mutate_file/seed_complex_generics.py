from typing import Dict, List, Tuple, Optional
import collections.abc as cabc


class Processor:
    def process(
        self,
        mapping: cabc.Mapping[str, List[Tuple[int, ...]]],
        /,
        *,
        key: Optional[str] = None,
    ) -> Dict[str, int]:
        result: Dict[str, int] = {}
        for k, v in mapping.items():
            if key is not None and k != key:
                continue
            result[k] = len(v)
        return result
