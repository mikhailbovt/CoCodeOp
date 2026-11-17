FROM docker.io/library/python@sha256:673719e071ecf27434fd2635bbd8d00f49f4fdd81f243143438a42ced8bedf60
COPY --chmod=755 office-runner /usr/local/bin/office-runner
COPY --chmod=755 generic_bridge_fixture.py /opt/cocodeop/bridge
