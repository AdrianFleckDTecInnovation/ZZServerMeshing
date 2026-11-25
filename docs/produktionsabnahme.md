# Infinite Stars betreiben und abnehmen

Ein Worker simuliert eine Bevy-World. Der **Octree** findet benötigte Nachbarn innerhalb eines Bezugssystems. Das **Directory** bündelt die angeforderten Komponenten je Empfänger. Der **Authority-Dienst** entscheidet auf seinem eigenen Datenträger, welcher Worker eine Gruppe verbindlich ändern darf. Bewegte Entities werden direkt an abonnierte Peers übertragen. Ein Worker darf mehrere weit entfernte Regionen bedienen. [Spielintegration](../crates/zz-mesh/README.md) · [Directory und Subscriptions](../crates/zz-mesh/src/spatial.rs) · [Kontrollspeicher](../crates/zz-mesh/src/authority_store.rs)

## Abgleich mit dem Auftrag

Am Ausgangsstand `6cc85be` funktionierten bereits Bevy-Integration, Voxelprovider, Aktionen, Handoff und ein räumlicher R-Baum. Vier Lücken betrafen den Einsatz im Spiel. Der geforderte Octree fehlte. Die Netzwerkworker benötigten Zugriff auf dieselbe Datenbankdatei. Die gemessene automatische Platzierung war auf den lokalen Pool beschränkt. Der Lasttest blendete den ersten Tick einer plötzlichen Gruppenverschmelzung aus. Der aktuelle Stand schließt diese Lücken und misst die Kosten der Übergänge separat.

| Abschnitte | Umsetzung im Spielpfad | Abnahme |
| --- | --- | --- |
| 1–2, 5, 18 · Einbindung | Bevy 0.15, eigener Library-Target, Game-Plugin, gewöhnlicher Spawn mit `Managed`, Schreibsysteme mit `With<Simulated>`. Ein eigenes Cargo-Projekt importiert die Bibliothek direkt aus Git. | `npm run test:game`, ausführbares Beispiel `survey_probe` und Neustart mit gespeichertem Stand. [Anschluss eines Spieltyps](../crates/zz-mesh/README.md) |
| 3–4 · Inseln und Interest | Pro Bezugssystem gibt es getrennte ganzzahlige 3D-Octrees für Aktivitätsbereiche und Objekte. Nur die Schnittmenge physischer Aktivitätsbereiche verbindet Inseln. Forecast, Hysterese, entfernte Referenzen, private Sichten und genaue Simulationsdaten haben eigene Regeln. | Vollständige Referenzsuche für zufällige Einfügungen, Bewegungen, Löschungen, Flächenkontakt, entfernte Ursprünge und Koordinatengrenzen. Wechsel und Merge prüfen auch installierte Bevy-Replicas. [Octree](../crates/zz-mesh/src/octree.rs) · [Interest-Tests](../crates/zz-mesh/tests/spatial.rs) |
| 6–7 · Lebenszyklus und Placement | Stabile Objekt-, Schema- und Gruppen-IDs, Revisionen, Tombstones und Referenzen. Pool und Netzwerk-Controller verwenden dieselbe Placement-Regel für gemessene Ticklast, Speicher, Cache, Kommunikation, Transferbudget und Mindestverweildauer. | Schemafehler, Gruppenänderungen, automatische Verlagerung von einem überlasteten Prozess, Drain, Sperren neuer Zuständigkeiten und Wiederaufnahme. [Placement](../crates/zz-mesh/src/placement.rs) · [Netzwerk-Controller](../examples/infinite-stars/src/supervisor.rs) |
| 8–10 · Aktionen und Spielzustand | Das Spiel registriert Inventar, Abbau, Bau, Drop, Pickup und Handel. Session und Sequenz überleben einen Socketwechsel. Genehmigungen mehrerer Writer, Aktionsresultat und Bestandsänderungen werden gemeinsam geprüft und gespeichert. Der Spielerclient führt seine Outbox und Vorhersagen lokal. | Wiederholung nach Commit vor Antwort, Abhängigkeiten und Vorräte, Handel zwischen Writern, private Felder und Abgleich von vorhergesagten Drops. [Spiel-Plugin](../examples/infinite-stars/src/lib.rs) · [Vorhersage](../examples/infinite-stars/src/prediction.rs) |
| 11–13 · Daten, Handoff und Recovery | Inhaltsadressierte Blobs, versionierte Provider, Teilübertragung mit Resume, Deltas, Cache-Pins und Rekonstruktion. Die Umschaltung bindet die Bereitschaft an den finalen Inhalt. Der Store prüft Generation und Ausgangsrevision in der Committransaktion. | Voxel- und Bauplanprovider, falsche Basis, fehlende Historie, verlorenes ACK, Abstürze, Wiederanlauf und erneute Zustellung desselben Belegs. [Bulk](../crates/zz-mesh/src/bulk.rs) · [Handoff](../crates/zz-mesh/src/handoff.rs) · [Recovery](../crates/zz-mesh/tests/recovery.rs) |
| 14–16 · Netzwerk und Last | Direkte typisierte Nachrichten im Pool, TLS mit beidseitiger Authentifizierung zwischen Prozessen. Separate Worker-Verzeichnisse, pro Inkarnation neu gebundene Prozessschlüssel, begrenzte Nachrichten und Warteschlangen. Der Speicherdienst führt dieselben registrierten Aktionshandler aus wie der direkte Store. | Eigene Ports und Kindprozesse, Dienstneustart, Handoff, paralleler Ersatz eines laufenden Workers, Ablehnung fremder privater Snapshots und gerichteter Netzverkehr. [Mehrprozesstests](../examples/infinite-stars/tests/process.rs) · [Transport- und Codecmessungen](infinite-stars-benchmarks.md) |
| 17 · M0 bis M7 | Die ursprüngliche Browser-Demo bleibt ausführbar. Die Beispiel-Crate verwendet die öffentliche Bibliothek im direkten Pool und über das Netzwerk. | Ein separates Bevy-Spiel kompiliert gegen die Git-Abhängigkeit. Windows und Debian durchlaufen dieselben Abnahmen. [Externer Integrationstest](../scripts/test-game-integration.mjs) · [Native CI](../.github/workflows/native-mesh.yml) |

## Auf getrennten Rechnern starten

Einmal `cargo build --release --locked -p infinite-stars --bin infinite-stars` ausführen. Die folgenden Befehle verwenden die erzeugte Binary über den Suchpfad. Die Adressen sind Beispiele. Das Spielbeispiel bringt einen Spieler mit einer festen Identität mit. Für eigene Konten erhält jeder Spieler einen eigenen Schlüssel. Der Resolver von `Listener::bind_resolved` ordnet den geprüften Zertifikatsfingerabdruck einer stabilen `Role::Player(actor_id)` zu. Damit bindet der Server Session, private Sicht und wirtschaftliche Aktionen an den jeweiligen Spieler. [Rollen und Serverprüfung](../crates/zz-mesh/src/wire.rs)

| Ort | Befehl |
| --- | --- |
| Authority-Rechner, einmalig | `infinite-stars provision control 1 worker-a` |
| Authority-Rechner, einmalig | `infinite-stars provision control 2 worker-b` |
| Authority-Rechner, einmalig | `infinite-stars provision-client control player` |
| Authority-Rechner | `infinite-stars authority control 10.20.0.10:8640` |
| Worker A | `infinite-stars worker worker-a 10.20.0.10:8640 10.20.0.11:8641` |
| Worker B | `infinite-stars worker worker-b 10.20.0.10:8640 10.20.0.12:8642` |
| Spieler | `infinite-stars client player 10.20.0.11:8641` |
| Authority-Rechner, Beispielgruppe an Slot 2 übergeben | `infinite-stars migrate control 10.20.0.11:8641 10.20.0.12:8642 2` |
| Authority-Rechner, Diagnose | `infinite-stars status control 10.20.0.12:8642` |
| Authority-Rechner, automatische Lastverteilung im Vordergrund | `infinite-stars balance control` |
| Authority-Rechner, Slot leeren und für neue Zuständigkeiten sperren | `infinite-stars drain control 2` |
| Authority-Rechner, Slot wieder freigeben | `infinite-stars resume control 2` |

Übertrage jedes erzeugte Verzeichnis geschützt an seinen Empfänger. `control` enthält Welt, Zulassungen, CA- und Operatorschlüssel und bleibt auf dem Authority-Rechner. Unter Windows die Zugriffsrechte des Dienstkontos für das Verzeichnis setzen. Unter Linux werden private Schlüssel mit Modus 0600 erzeugt. Bei Bindung an eine Wildcard gehört die für Peers erreichbare Adresse als `IP:Port` in `advertise.txt` im Worker-Verzeichnis. Slot 1 betreibt das Directory. Eine weitere Worker-Instanz erhält einen eigenen Slot und ein eigenes Verzeichnis.

Die Ersteinrichtung veröffentlicht Schlüssel und Zertifikate erst nach dem vollständigen Schreiben. Ein Neustart setzt eine unterbrochene Ausgabe mit demselben Schlüssel fort. Die Ausgabe in ein neues Verzeichnis wartet unter Windows kurz auf vorübergehende Dateisperren. Ein bereits veröffentlichtes Ziel bleibt geschützt. Bei einem Prozessabbruch zwischen Verzeichnisexport und Zulassungs-Commit kann die Anmeldung mit dem exportierten Worker-Schlüssel scheitern. Dann denselben Slot in ein frisches Ziel provisionieren.

Bei jedem Prozessstart erzeugt der Worker ein neues Schlüsselpaar. Der Authority-Dienst prüft die Zulassung für den Slot, signiert den öffentlichen Schlüssel und bindet dessen Fingerabdruck atomar an die neue Inkarnation. Ein Ersatz desselben Slots sperrt alte Commits. Der Spieler behält seinen lokalen Outbox-Datenträger. Ein erneutes `provision` für einen Slot in ein neues Zielverzeichnis rotiert dessen Zulassung. [Bereitstellung](../examples/infinite-stars/src/deployment.rs) · [Spielserver](../examples/infinite-stars/src/host.rs)

## Betrieb und Grenzen

Der Controller fragt alle zwei Sekunden die registrierten Prozesse ab. `balance control --once` führt einen einzelnen Durchgang aus. Eine fachliche Gruppe wechselt bei Bedarf als Ganzes. Der Controller berücksichtigt angeforderte Komponenten je Peer und verifizierte Cacheinhalte am Ziel. Standardwerte sind 40 ms Tickarbeit, 256 MiB Cachebudget, 8 MiB Transferbudget und vier Sekunden Mindestverweildauer. Ein Worker kann seine beiden Kapazitätsbudgets per `capacity.json` im eigenen Verzeichnis setzen, beispielsweise `{"tick_budget_us":25000,"memory_bytes":134217728}`. Das Speicherbudget erfasst den Blobcache. Zusätzliche Bevy- und Spielallokationen müssen bei der Dimensionierung des Hosts berücksichtigt werden. Gibt es kein geeignetes Ziel, meldet der Controller die Gruppe und der bisherige Writer behält die Zuständigkeit.

- **Ausfall eines Workers.** Mit demselben Slot neu starten. Der Worker stellt seine Gruppen einschließlich bestätigter wirtschaftlicher Änderungen wieder her. Beim geordneten Handoff gilt der exakte Endstand. Kontinuierliche Daten starten nach Absturz beim letzten haltbaren Checkpoint. [Haltbarkeitsregeln](infinite-stars.md)
- **Ausfall des Authority-Dienstes.** Schreibende Operationen brauchen dessen Bestätigung. Den Dienst an seiner bisherigen Adresse mit dem intakten Datenträger wieder starten. Eine ungeklärte Aktion mit derselben Session und Sequenz wiederholen. Bei unklarem Handoff dasselbe Ticket fortsetzen oder über `abort` den gespeicherten Ausgang klären. Der Beispiel-Controller führt dafür eine haltbare Ticketdatei. [Prozessabnahme](../examples/infinite-stars/tests/process.rs)
- **Datenträgerverlust.** Der Standarddienst hat eine maßgebliche SQLite-WAL-Datenbank. Für ein konsistentes kaltes Backup Authority und Verwaltungsbefehle beenden und das ganze geschützte `control`-Verzeichnis sichern. Ein einzelnes Volume bietet keine automatische Ausfallsicherung bei Verlust des Rechners. Wiederherstellung aus Backup hat den Datenstand des Backups.
- **Dichte und Kapazität.** Echte Sichtbarkeit zwischen allen Teilnehmern verursacht weiterhin Datenverkehr. Ein Objekt hat pro interessiertem Worker höchstens eine zusammengefasste Subscription. Individuelle Clients können jeweils eine eigene Sicht benötigen. Der Standardbroker unterstützt bis zu 64 Worker-Slots. Diese Grenze zählt Prozesse. Die getesteten 1.024 Spielfiguren sind bewegte Bevy-Entities. [Lastprofil und Messgrößen](cluster-performance.md)
- **Betriebsumgebung.** Der kurze Test prüft das Modell vertrauenswürdiger Worker. Die Standardbereitstellung benötigt eine erreichbare Authority und für neue Routen ein erreichbares Directory. Automatische Bereitstellung weiterer Maschinen, redundanter Speicher und ein Kapazitätsziel mit dem eigenen Gameplay sind Entscheidungen des Betreibers.

## Abnahme wiederholen

```sh
npm test
npm run test:game
npm run smoke
cargo run --release --locked -p infinite-stars --bin cluster-bench -- target/islands.json --quick
npm run bench:distributed
cargo run --release --locked -p infinite-stars --bin mesh-bench -- target/world.json
npm run bench:compare -- --npcs 3000 --seconds 15 --repeats 1
```

Der direkte Lasttest prüft nach jedem Schritt die Inseln und den Zustand jeder Replica gegen eine erschöpfende Suche. Prüfzeit und Erzeugung der Testwelt liegen außerhalb der ausgewiesenen Schrittkosten. Im Netztest endet eine Messung erst, wenn alle Empfänger den erwarteten Stand melden. Peer-/Directory-TLS und der Datenverkehr zum Authority-Dienst werden gesondert gezählt. [Szenen und Perzentile](../examples/infinite-stars/src/bin/cluster-bench.rs) · [Netzwerkbenchmark](../examples/infinite-stars/src/bin/cluster-wire.rs)

Am 25. November 2026 bestanden unter Windows und Debian jeweils **684 Rust-Tests, 85 TypeScript-Prüfungen, der Client-Produktionsbuild und beide Live-Smokes**. Das separat kompilierte Bevy-Spiel lud nach dem Neustart den gespeicherten Reaktorstand. Der Abhängigkeitsscan meldete für 290 Rust-Pakete und die vier npm-Lockfiles null bekannte Schwachstellen. RustSec führt Wartungshinweise für `basic-toml`, `bincode` und `rustls-pemfile`. Das getestete Fehlermodell umfasst Prozessabsturz, Retry, Partition und veraltete Writer. [Prüfstand und Advisory-Version](benchmarks/octree-pruefstand.json) · [Leistungsnachweis](octree-messung.md)
