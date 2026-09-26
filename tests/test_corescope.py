from rf_router_planner.integrations.corescope import CoreScopeClient


class Response:
    status_code = 200
    headers: dict[str, str] = {}

    def __init__(self, payload) -> None:  # type: ignore[no-untyped-def]
        self.payload = payload

    def json(self):  # type: ignore[no-untyped-def]
        return self.payload

    def raise_for_status(self) -> None:
        return None


class Session:
    def __init__(self, pages) -> None:  # type: ignore[no-untyped-def]
        self.pages = iter(pages)

    def get(self, *args, **kwargs):  # type: ignore[no-untyped-def]
        return Response(next(self.pages))


def test_corescope_import_filters_invalid_and_non_repeater_nodes() -> None:
    payload = {
        "nodes": [
            {
                "public_key": "a" * 64,
                "name": "Hilltop",
                "role": "repeater",
                "lat": 60.1,
                "lon": 10.2,
                "last_heard": "2026-08-31T08:09:44Z",
                "relay_active": True,
                "relay_count_24h": 12,
            },
            {"public_key": "b" * 64, "role": "repeater", "lat": 0, "lon": 0},
            {"public_key": "c" * 64, "role": "companion", "lat": 60, "lon": 10},
        ],
        "total": 3,
    }
    result = CoreScopeClient(session=Session([payload])).fetch_repeaters()
    assert len(result) == 1
    assert result[0].name == "Hilltop"
    assert result[0].relay_count_24h == 12
