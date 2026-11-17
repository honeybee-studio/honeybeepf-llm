# Run without Kubernetes

The agent also runs as a plain binary on a Linux host, such as a VM that runs your LLM apps. It records the same calls;
the metrics carry no namespace, pod or workload labels.

## 1. Get the binary

[Build it](build.md) for a Linux host, then copy it to the server:

=== "scp"

    ```bash
    scp target/release/honeybeepf-llm <user>@<server>:/tmp/
    ssh <user>@<server> 'sudo install -m 0755 /tmp/honeybeepf-llm /usr/local/bin/honeybeepf-llm'
    ```

=== "xtask deploy"

    Builds, copies and installs to `/usr/local/bin/honeybeepf-llm` (`--path` to change it):

    ```bash
    cargo +nightly xtask deploy --host <user>@<server> --release
    ```

=== "Tarball"

    ```bash
    cargo +nightly xtask package --output dist
    scp dist/honeybeepf-llm-*.tar.gz <user>@<server>:/tmp/
    ssh <user>@<server> 'cd /tmp && tar xzf honeybeepf-llm-*.tar.gz && cd honeybeepf-llm-* && sudo ./install.sh'
    ```

## 2. Try it

The host needs Linux 5.10 or later, or a RHEL 8.10, 9.6 or 9.7 kernel (`uname -r`).

```bash
sudo BUILTIN_PROBES__LLM=true honeybeepf-llm --verbose
```

!!! warning "Turn the LLM probe on"
    The binary starts with no probe attached unless `BUILTIN_PROBES__LLM=true` is set. The chart sets it for you; on
    a host you set it yourself. `--verbose` (or `RUST_LOG=info`) shows the `LLM SUCCESS` line of each call.

Make an LLM call from another terminal on the same host; a line with the model and the token counts appears.

## 3. Run it as a service

=== "xtask"

    From the machine you build on, install a systemd unit on the server that turns the LLM probe on, logs each call
    and keeps the six capabilities below:

    ```bash
    cargo +nightly xtask install-service --host <user>@<server>
    ssh <user>@<server> 'sudo systemctl start honeybeepf-llm'
    ```

    To send metrics to a Collector, put `OTEL_EXPORTER_OTLP_ENDPOINT=...` in an environment file on the server and pass
    it with `--config /etc/honeybeepf-llm/honeybeepf-llm.env`; its settings override the unit's.

=== "By hand"

    Put the settings in an environment file:

    ```bash
    sudo mkdir -p /etc/honeybeepf-llm
    sudo tee /etc/honeybeepf-llm/honeybeepf-llm.env <<'EOF'
    BUILTIN_PROBES__LLM=true
    RUST_LOG=info
    # Send metrics to an OpenTelemetry Collector; leave it out to keep the calls in the log only.
    # OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
    EOF
    ```

    Then a systemd unit that runs the agent as root:

    ```bash
    sudo tee /etc/systemd/system/honeybeepf-llm.service <<'EOF'
    [Unit]
    Description=honeybeepf-llm LLM eBPF agent
    After=network.target

    [Service]
    Type=simple
    ExecStart=/usr/local/bin/honeybeepf-llm
    EnvironmentFile=/etc/honeybeepf-llm/honeybeepf-llm.env
    Restart=on-failure
    RestartSec=5

    [Install]
    WantedBy=multi-user.target
    EOF
    sudo systemctl daemon-reload
    sudo systemctl enable --now honeybeepf-llm
    sudo journalctl -u honeybeepf-llm -f
    ```

## Permissions

The agent loads eBPF programs and reads other processes' TLS libraries, so it runs as root, or as a user with the
same capabilities the chart grants:

| Capability | Why |
|---|---|
| `CAP_BPF` | load eBPF programs and maps |
| `CAP_PERFMON` | attach uprobes and tracepoints and read user memory |
| `CAP_SYS_ADMIN` | open uprobe perf events (some kernels, such as Ubuntu's 7.0, deny it with `CAP_PERFMON` alone) |
| `CAP_SYS_PTRACE` | read other processes' `/proc` to find their `libssl` |
| `CAP_NET_ADMIN` | create the plaintext socket map at start, on kernels before 6.9 |
| `CAP_SYS_RESOURCE` | raise the memlock limit for the BPF maps, on kernels before 5.11 |

```bash
sudo setcap 'cap_bpf,cap_perfmon,cap_sys_admin,cap_sys_ptrace,cap_net_admin,cap_sys_resource+ep' \
  /usr/local/bin/honeybeepf-llm
```

The [security review](security-review.md#2-privileges) explains each one.

## Check and remove

```bash
sudo systemctl status honeybeepf-llm
sudo bpftool prog list | grep -i probe      # the loaded eBPF programs
```

To remove it:

```bash
sudo systemctl disable --now honeybeepf-llm
sudo rm /etc/systemd/system/honeybeepf-llm.service /usr/local/bin/honeybeepf-llm
sudo rm -rf /etc/honeybeepf-llm
sudo systemctl daemon-reload
```
