from gpuwho import demo


def test_demo_cluster_moves():
    servers = {s.name: s for s in demo.servers()}
    node01, node03 = servers["node01"], servers["node03"]
    assert [p["user"] for p in node01.data["gpus"][2]["procs"]] == ["bob"]
    for _ in range(7):
        for s in servers.values():
            s.probe()
    assert node01.data["gpus"][2]["procs"] == []  # bob's job finished at poll 4
    assert [p["user"] for p in node03.data["gpus"][3]["procs"] if p["type"] == "C"] == ["frank"]
    assert node01.version == 7 and node01.due()
    for g in node01.data["gpus"]:
        assert 0 <= g["util"] <= 100 and 28 <= g["temp"] <= 87
