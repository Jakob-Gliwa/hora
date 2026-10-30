# Additional HTTP and ICMP settings

HTTP supports `expected_statuses = [200, 201, 302]` as an alternative to the
existing single `expected_status`. The migration tool expands status ranges.
Redirects are followed unless all explicitly accepted codes are redirects.
`method = "HEAD"` overrides GET. `ignore_tls = true` explicitly disables
certificate verification for this monitor's HTTP requests. Its pool is kept
separate from verified requests, including peer probes and dual-stack checks.
This option does not disable the independent certificate expiry watch; set
`check_cert = false` if that is also intended.

ICMP supports `ping_count = 3` (1..10) and `packet_size = 56` (1..4096 payload
bytes). At least one echo reply passes; latency is the average of replies.
Packets are spaced by one second and share the monitor's timeout budget.
Defaults remain one packet and 16 payload bytes.
