def test_terrain_can_be_imported_before_rf_propagation() -> None:
    from rf_router_planner.terrain.raster import ArrayTerrain

    assert ArrayTerrain is not None
