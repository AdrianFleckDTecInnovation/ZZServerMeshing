# Infinite-Stars-Benchmark v1

Die erneute Messung der Browser-Demo und die Mehrspieler-Szenen für getrennte, wechselnde und verschmelzende Cluster stehen im [Cluster-Benchmark](cluster-performance.md).

Messung vom 24. November 2026 auf einem Intel Core i7-13850HX. Windows 11 x64 lief nativ. Debian 12 lief auf demselben Rechner in einer QEMU-VM mit Windows-Hypervisor, sechs vCPUs und 8 GiB RAM. Beide Läufe verwendeten Rust 1.96.1 und Release-Binärdateien. Die Zahlen sind Einzelmessungen für die jeweilige Konfiguration. Die JSON-Dateien enthalten die vollständigen Zähler. [Windows](benchmarks/infinite-stars-v1-windows.json) · [Debian](benchmarks/infinite-stars-v1-linux.json)

## Reproduzieren

```sh
cargo run --release --locked -p infinite-stars --bin mesh-bench -- target/stars-benchmark.json
```

Der Benchmark legt ein frisches, laufbezogenes Verzeichnis und eigene Loopback-Ports an. Er beendet die selbst gestarteten Worker nach der Messung. Die räumlichen Szenen verwenden die echte Bibliothek mit Bevy-Worlds und einem In-Memory-Kontrollspeicher. Die Handoff- und Aktionsmessungen verwenden SQLite-Dateien und vollständige Commits. [Szenen und Messcode](../examples/infinite-stars/src/bin/mesh-bench.rs)

## Räumliche Arbeit

Auf sechs Warmlaufschritte folgen 160 gemessene Schritte. Es gibt 256 Objekte je unabhängiger Insel und vier deterministische Positionsänderungen je Insel und Schritt. Die ersten drei Szenen verteilen dieselben 256 Objekte am selben Ort. Die letzten beiden erhöhen Arbeit und Workerzahl proportional. Der lokale Pool tickt seine Worlds seriell. Die Zeit umfasst Mutation, ECS, Discovery, Projektion, Zustellung, ACK und die Messzähler. Die Perzentile verwenden den nächsten Rang. Kernel-Scheduling und Checkpoints sind im jeweiligen Schritt enthalten.

| System | Worker | Inseln | Objekte | p50 ms | p95 ms | p99 ms | Queue-Spitze |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Windows 11 | 1 | 1 | 256 | 0.044 | 0.338 | 0.905 | 5 |
| Windows 11 | 2 | 1 | 256 | 0.078 | 0.257 | 1.316 | 9 |
| Windows 11 | 4 | 1 | 256 | 0.086 | 0.674 | 1.014 | 12 |
| Windows 11 | 4 | 4 | 1024 | 0.199 | 0.738 | 3.895 | 19 |
| Windows 11 | 16 | 16 | 4096 | 0.833 | 3.170 | 13.801 | 72 |
| Debian 12 | 1 | 1 | 256 | 0.038 | 0.124 | 0.775 | 5 |
| Debian 12 | 2 | 1 | 256 | 0.056 | 0.175 | 0.856 | 9 |
| Debian 12 | 4 | 1 | 256 | 0.081 | 0.288 | 1.180 | 12 |
| Debian 12 | 4 | 4 | 1024 | 0.186 | 0.635 | 3.431 | 19 |
| Debian 12 | 16 | 16 | 4096 | 0.812 | 2.920 | 10.934 | 72 |

Der Beobachter auf Insel 1 erhielt in allen fünf Szenen je System genau **697 Frames mit 17.101 Byte Komponentendaten**. Der Benchmark prüft diese Gleichheit für die unabhängigen Inseln. Die ACK-Störung ist an Objekt und Schritt gekoppelt. Wiederholungen sind mitgezählt. Der direkte Pfad belegt dafür null Netzwerkbytes. Ein Zufallstest vergleicht den Index zusätzlich mit einer vollständigen 3D-Suche. Ein Drei-Prozess-Test misst null empfangene und null gesendete Entity-Frames beim entfernten Worker. [Räumliche Tests](../crates/is-mesh/tests/spatial.rs) · [Mehrprozesstests](../examples/infinite-stars/tests/process.rs)

| System | Worker | Abos | gerichtete Paare | räumliche Kandidaten | Wiederholungen | Allokationen | allokierte Byte | zusätzliche Heap-Spitze, Byte |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Windows 11 | 1 | 256 | 1 | 1414 | 57 | 22528 | 4916405 | 125150 |
| Windows 11 | 16 | 4096 | 16 | 22624 | 904 | 357776 | 81410220 | 750928 |
| Debian 12 | 1 | 256 | 1 | 1414 | 57 | 22528 | 4916405 | 124594 |
| Debian 12 | 16 | 4096 | 16 | 22624 | 904 | 357776 | 81410220 | 750928 |

Der Zähler um den Systemallocator misst den Prozessheap ab Ende des Warmlaufs. Die Heap-Spitze ist zusätzlicher gleichzeitig belegter Speicher. Kumulierte Allokationen umfassen auch kurzlebige Änderungen und Belege. Die räumlichen Kandidaten sind exakte Treffer des Broadphase-Index einschließlich anschließender Filterarbeit. Direkte Empfängerzahl, projizierte Felder, ausstehende Kanten und Wiederholungen stehen getrennt in den Rohdaten. Die Betriebsziele für diese Szene sind ein 50-ms-Schritt, unter 2 MiB zusätzlicher Heap nach dem Warmlauf und weniger als 256 ausstehende Zustellungen. Die exakte Verkehrsgleichheit wird in CI geprüft. Maschinenabhängige Laufzeitziele lassen sich anhand der gespeicherten CI-Artefakte vergleichen.

## Übergabe einer Spielgruppe

Die Gruppe umfasst Spieler, Planet, Chunk und Baugruppe. Die Pflichtdaten bestehen aus einem 4.096-Byte-Chunk und einem 16-Byte-Bauplan. Die Vorbereitung beginnt mit dem Transferauftrag. Die Pause umfasst Einfrieren, finale Bereitschaft, haltbares Umschalten und Aktivieren. Der Netzwerkwert misst diese Schritte aus Sicht des aufrufenden Controllers. Jede Zeile des Netzwerkfalls beinhaltet zwei neue TLS-Verbindungen. Die TX/RX-Werte messen beide Verbindungen aus Controllersicht.

| System | Pfad | Zielcache | Läufe | Bulk-Byte | Vorbereitung ms, Median | Pause ms, Median | TX Byte | RX Byte |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Windows 11 | lokal | kalt | 1 | 4112 | 2.931 | 1.615 | 0 | 0 |
| Windows 11 | lokal | warm | 5 | 0 | 0.485 | 1.218 | 0 | 0 |
| Windows 11 | TCP/mTLS | kalt | 1 | 4112 | 6.066 | 7.351 | 8037 | 7895 |
| Windows 11 | TCP/mTLS | warm | 2 | 0 | 7.340 | 6.331 | 3215 | 3392 |
| Debian 12 | lokal | kalt | 1 | 4112 | 1.676 | 4.347 | 0 | 0 |
| Debian 12 | lokal | warm | 5 | 0 | 1.450 | 4.584 | 0 | 0 |
| Debian 12 | TCP/mTLS | kalt | 1 | 4112 | 7.448 | 9.216 | 8036 | 7896 |
| Debian 12 | TCP/mTLS | warm | 2 | 0 | 6.550 | 8.731 | 3215 | 3395 |

Ein warmer Rückweg benötigt weiterhin Kontrollnachrichten und die kanonischen Komponenten. Die warmen Läufe haben null zusätzliche Blob-Nutzlast. Das Beispiel transportiert die kompakten Abbau- und Baumodifikationen des Chunks auch als wirtschaftlich verbindlichen Komponentenzustand. Der Wert für Bulk-Byte zählt ausschließlich die verifizierten Provider-Daten.

## Aktionen, Transport und Codecs

Pro Pfad verarbeitet derselbe Handler 192 unterschiedliche Drop-Aktionen mit insgesamt 768 Zielen. Acht Aktionen teilen sich jeweils einen Request. Drei Transportbatches werden unverändert erneut angefordert. Die 27 Aufrufe enthalten die Wiederholungen. Die Socketzähler erfassen TLS-Handshake, Framing, Nutzdaten und wiederholte Anwendungsnachrichten. Ethernet-, IP- und TCP-Header sowie mögliche Wiederholungen im Betriebssystem sind außerhalb dieser Socketmessung.

| System | Pfad | Aufrufe | p50 ms | p95 ms | TX Byte | RX Byte | kodierte Requests, Byte | kodierte Antworten, Byte |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Windows 11 | direct | 27 | 1.573 | 2.026 | 0 | 0 | 0 | 0 |
| Windows 11 | tcp_mtls | 27 | 1.809 | 4.783 | 11633 | 23985 | 9990 | 22174 |
| Debian 12 | direct | 27 | 3.168 | 3.912 | 0 | 0 | 0 | 0 |
| Debian 12 | tcp_mtls | 27 | 2.713 | 5.272 | 11635 | 23984 | 9990 | 22174 |

Die Rohdaten berichten Encoding und Decoding als Thread-CPU-Zeit. Windows liefert zusätzlich Thread-Zyklen, da der CPU-Zeitgeber bei einzelnen kurzen Nachrichten null messen kann. Separate Codec-Schleifen führen pro Datenform 512 Encodes und Decodes aus. Die Größen der tatsächlich belegten Framing-Nutzdatenpuffer erfassen die für den Transport materialisierten Bytes. Eine Frame-Erstellung schreibt Header und Inhalt in einen gemeinsamen Puffer. TLS-interne Kopien sind nicht einzeln instrumentiert.

| System (je 512 identische Owner-Abfragen) | Transport | TX Byte | RX Byte | p50 ms | p95 ms |
| --- | --- | --- | --- | --- | --- |
| Windows 11 | tcp_mtls | 25872 | 19378 | 0.044 | 0.063 |
| Windows 11 | udp_loopback | 8704 | 2048 | 0.037 | 0.068 |
| Debian 12 | tcp_mtls | 25872 | 19379 | 0.087 | 0.246 |
| Debian 12 | udp_loopback | 8704 | 2048 | 0.057 | 0.144 |

Für die Transportgegenprobe dekodiert auch der UDP-Empfänger denselben binären Request und führt denselben Owner-Lookup aus. UDP verwendet für diese isolierte Loopback-Messung eine feste Rolle. Der Standardadapter verwendet TCP/mTLS für zertifikatsgebundene Rollen, begrenzte Streams und vollständige Blöcke. Aktionen behalten ihre fachliche Deduplizierung und Zustandsdeltas ihre bestätigten Baselines. Bincode und JSON wurden an denselben Voxeländerungen gemessen. Der Binärcodec gehört zum versionierten Standardadapter.

| geänderte Zellen von 4.096 | sortierte Indizes, Byte | Bereiche, Byte | Maske und Palette, Byte | kleinstes JSON, Byte |
| --- | --- | --- | --- | --- |
| 2 | 8 | 10 | 521 | 28 |
| 16 | 64 | 80 | 531 | 151 |
| 256 | 996 | 1252 | 621 | 1419 |
| 1024 | 3974 | 4998 | 911 | 2928 |
| 4096 | 15886 | 1252 | 2063 | 3011 |

Bei zwei geänderten Zellen sind sortierte Indizes am kleinsten. Bei 256 verteilten Änderungen ist die Palette mit Maske kleiner. Bei vollständig belegten, gleichartigen Bereichen gewinnen Läufe. Der Provider vergleicht auch ein zusammenhängendes Ersetzungsstück mit diesen Formaten und verwirft Deltas, deren Basis oder Zielhash nicht stimmt.

## Browser-Demo

Die bestehende 20-Hz-Browser-Demo wurde zusätzlich mit drei Servern, drei bestätigenden WebSocket-Clients, 3.000 NPCs, fünf Sekunden Warmlauf und 15 Sekunden Messzeit gestartet. Die Tabellen im ursprünglichen Bericht enthalten die 0.18-Ausgangswerte. Die folgenden Zahlen stammen vom gemeinsamen Bevy-0.15-Kern. [Ausgangsmessungen](benchmark.md) · [Rohdaten nach dem Umbau](benchmarks/browser-bevy015.json)

| System | niedrigste Tickrate, Hz | niedrigste Snapshotrate, Hz | höchste p95 ms | höchste p99 ms |
| --- | --- | --- | --- | --- |
| Windows 11 | 19.982 | 19.982 | 4.908 | 5.938 |
| Debian 12 | 19.995 | 19.995 | 3.008 | 8.476 |
