from rf_router_planner.models.settings import RFSettings
from rf_router_planner.models.site import Site, SiteKind, SiteOrigin
from rf_router_planner.project import Project, load_project, save_project


def test_project_round_trip(tmp_path) -> None:
    project = Project(
        endpoint_a=Site("A", 1, 2, 60, 10, SiteKind.ENDPOINT_A), rf_settings=RFSettings()
    )
    project.terrain_settings.auto_resolution = True
    project.terrain_settings.maximum_total_pixels = 75_000_000
    project.terrain_settings.maximum_download_tiles = 512
    project.terrain_settings.detail_resolution_m = 5
    project.terrain_settings.detail_corridor_width_m = 750
    project.additional_clients.append(Site("C3", 3, 4, 61, 11, SiteKind.CLIENT))
    project.manual_routers.append(
        Site(
            "M1",
            5,
            6,
            60.5,
            10.5,
            SiteKind.ROUTER,
            origin=SiteOrigin.MANUAL,
            required=True,
        )
    )
    path = tmp_path / "test.rfplan.json"
    save_project(project, path)
    loaded = load_project(path)
    assert loaded.endpoint_a is not None
    assert loaded.endpoint_a.latitude == 60
    assert loaded.rf_settings.frequency_mhz == 869.5
    assert loaded.terrain_settings.maximum_total_pixels == 75_000_000
    assert loaded.terrain_settings.maximum_download_tiles == 512
    assert loaded.terrain_settings.detail_resolution_m == 5
    assert loaded.terrain_settings.detail_corridor_width_m == 750
    assert [site.id for site in loaded.additional_clients] == ["C3"]
    assert loaded.manual_routers[0].required
    assert loaded.manual_routers[0].origin == SiteOrigin.MANUAL


def test_version_one_project_still_loads(tmp_path) -> None:
    import json

    path = tmp_path / "legacy.rfplan.json"
    path.write_text(
        json.dumps(
            {
                "format": "rf-router-planner",
                "version": 1,
                "endpoint_a": {
                    "id": "A",
                    "x": 1,
                    "y": 2,
                    "kind": "endpoint_a",
                },
            }
        ),
        encoding="utf-8",
    )
    loaded = load_project(path)
    assert loaded.endpoint_a is not None
    assert loaded.endpoint_a.id == "A"
    assert loaded.additional_clients == []
