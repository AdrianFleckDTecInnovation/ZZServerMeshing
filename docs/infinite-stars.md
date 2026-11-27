# Infinite Stars mit Bevy 0.15

Für den Einstieg in ein eigenes Spiel gibt es ein kompaktes Beispiel mit einer persistenten Komponente. Dieser Leitfaden beschreibt anschließend den Betrieb mehrerer Worker und die Erweiterung um eigene Aktionen, Daten und Abonnements. [Spielintegration](../crates/is-mesh/README.md) · [Ausführbarer Einstieg](../crates/is-mesh/examples/persistent_game.rs) · [Clustermessungen](cluster-performance.md)

`is-mesh` 1.0 unterstützt lokale Bevy-Simulation ohne Datenbank sowie dauerhaft koordinierte Worker. Das externe Sessionbeispiel führt Zulassung, individuelle Zustellung, Migration und Wiederanlauf mit eigenen Typen aus. [Versionierte Integration](../crates/is-mesh/README.md) · [Modulabnahme](mesh-v1-abnahme.md)

Im persistenten Referenzspiel gilt folgendes Modell. Ein Worker besitzt eine Bevy-World. Eine Gruppe kann mehrere Entities enthalten. Ein Worker darf viele Gruppen in verschiedenen Regionen simulieren. Die eigenständige Beispiel-Crate implementiert Spieler, Inventare, Werkzeuge, Planeten, Chunks, zusammengesetzte Voxelobjekte, Abbau, Bau, Drop, Pickup und Handel. [Bibliothek](../crates/is-mesh/src/lib.rs) · [Spiel-Plugin](../examples/infinite-stars/src/lib.rs)

## Starten

Rust stable und der native C-Linker reichen für das Beispiel. Unter Windows werden die MSVC-Toolchain und das Windows SDK verwendet. Alle Befehle laufen im Repository-Stamm unter Windows und Linux.

```sh
cargo run --locked -p infinite-stars
cargo run --locked -p infinite-stars --example survey_probe
```

Der erste Befehl öffnet `target/infinite-stars/world.sqlite`, startet zwei lokale Bevy-Worlds, prüft die Daten und übergibt die Spielgruppe. Ein weiterer Aufruf öffnet den gespeicherten Stand erneut. Das Programm meldet Generation, Bulk-Nutzlast, Vorbereitungsdauer und Pause. Das zweite Beispiel definiert eine Sonde mit zehn Einheiten Ladung. Eine typisierte Aktion scannt drei Kanäle in einem Commit und lässt sieben Einheiten übrig. [Ausführbares Sondenbeispiel](../examples/infinite-stars/examples/survey_probe.rs)

Für getrennte Prozesse vier Terminals verwenden. Die ersten zwei Befehle bleiben im Vordergrund. Mit Strg+C endet der jeweilige Prozess.

```sh
cargo run --locked -p infinite-stars -- serve target/stars-net 1 127.0.0.1:8642
cargo run --locked -p infinite-stars -- serve target/stars-net 2 127.0.0.1:8643
cargo run --locked -p infinite-stars -- client target/stars-net 127.0.0.1:8642
cargo run --locked -p infinite-stars -- migrate target/stars-net 127.0.0.1:8642 127.0.0.1:8643 2
```

Ein zusätzlicher Worker erhält einen freien Slot und eine freie Adresse, beispielsweise `serve target/stars-net 3 127.0.0.1:8644`. Slot 1 betreibt zusätzlich das räumliche Directory. Der Client fragt beim Einstieg nach dem aktuellen Writer und lädt den bestätigten Anfangsstand. Sein SQLite-Outbox-Eintrag enthält Session, nächste Sequenz, bestätigten Zustand und offene Absichten. Nach einem Verbindungsabbruch kann derselbe Clientbefehl fortgesetzt werden. Das Beispiel verwendet einen gemeinsamen lokalen Speicherordner für die Worker. Die Entwicklungs-CA und die private Spieleridentität liegen in diesem Ordner. Für getrennte Hosts erzeugt `provision` ein eigenes Verzeichnis je Worker. Der Authority-Dienst öffnet den Weltspeicher. [Start auf mehreren Rechnern](produktionsabnahme.md) [Prozessadapter](../examples/infinite-stars/src/host.rs) · [Client](../examples/infinite-stars/src/main.rs)

## Eigene Spieltypen anschließen

```rust
app.add_plugins((MinimalPlugins, MeshPlugin::default(), SurveyPlugin));
commands.spawn((Managed::default(), Probe { charge: 10, calibration }));
```

`MeshPlugin::default()` richtet einen lokalen Worker mit einer In-Memory-Datenbank ein. `MeshPlugin::persistent(store, worker)` verwendet einen benannten Slot und haltbaren Speicher. `Managed::default()` vergibt eine stabile ID und eine eigene Gruppe. `Managed::new(id, group)` drückt eine fachlich zusammengehörige Simulationseinheit aus. Schreibende Systeme verwenden `With<Simulated>`. Vorbereitete Entities tragen `Staged`, lokale Kopien `Replica`. [Bevy-Integration](../crates/is-mesh/src/bevy_runtime.rs) · [Bewegungssystem](../examples/infinite-stars/src/movement.rs)

Das Fachplugin registriert den Typ per `mesh_component` mit numerischer Schema-ID, Version, Sichtbarkeit, Migrations- und Persistenzregel. Der Standardcodec arbeitet mit Serde. Ein eigener Codec projiziert Sichten, beschreibt stabile Referenzen und löst lokale Bevy-Handles beim Import auf. `RequiredLocal` muss auflösbar sein. `OptionalLocal` darf fehlen. `Remote` erhält die dauerhafte Identität über Entfernung hinweg. Nicht registrierte Komponenten können im Spiel lokal verwendet werden. Das Beispiel löst Bauteile beim Import neu auf, rekonstruiert den Collider und übernimmt Ladung und Flüssigkeitsmenge exakt. [Komponenten und Regeln](../examples/infinite-stars/src/lib.rs)

`mesh_datasets` beschreibt benötigte Daten aus einer Komponente, `mesh_provider` registriert deren Beschaffung und Prüfung. Eine Entity kann dadurch ihr kalibriertes Artefakt oder ihren adressierten Chunk anfordern. `mesh_action` registriert einen typisierten Handler. Er erhält den authentifizierten Akteur und einen begrenzten `Edit` mit Lesezugriff auf eigene, bereits vorgemerkte Änderungen. `commit_batch` ist der Einstieg für serverseitige Aktionen aus einem Bevy-System. Neue Typen, Aktionen und Provider im Sondenbeispiel erfordern Änderungen ausschließlich in der Anwendung. [Sonde, Aktion und Datenprovider](../examples/infinite-stars/examples/survey_probe.rs)

## Räumliche Verteilung

Positionen und Ausdehnungen verwenden vorzeichenbehaftete 64-Bit-Millimeter in einem stabil benannten Bezugssystem. Pro Bezugssystem gibt es zwei dynamische 3D-Octrees für Objekte und Interessen. Auch sehr große Boxen und Berührungen an ihren Grenzen werden ganzzahlig geprüft. Innerhalb eines Ticks bündelt das Directory die Änderungen aller betroffenen Sichtbereiche je Objekt. Ein Worker kann mehrere räumlich getrennte Regionen melden. Planeten haben im Beispiel eine Katalogidentität, die adressierbaren Chunks beschreiben die lokalen Geometrien. [Octree](../crates/is-mesh/src/octree.rs) · [Räumliche Referenztests](../crates/is-mesh/tests/spatial.rs)

Die physischen Überlappungen aktiver Regionen bilden Inseln. Ein gerichtetes Interesse enthält Empfänger, Objekt, Komponentensicht, Genauigkeit und Aktualisierungsperiode. Bewegte Interessen verwenden eine vorausberechnete Hülle. Für Reisevorbereitung und Sensoren kann das Spiel eine Region mit `activity = false` anmelden. Erweiterte Austrittsgrenzen halten bestehende Kanten bei Grenzbewegungen stabil. Gleiche Interessen eines Peers werden zusammengefasst. Eine neue Inselzugehörigkeit allein erzeugt weder eine neue Objekt-ID noch eine neue Authority. [Räumliche Regeln](../crates/is-mesh/src/spatial.rs)

Der Sternflug mit zwei Workern und je 100 Spielern zeigt die Ein- und Austrittsgrenzen, die Aufteilung in 101 Inseln und die Kosten der Nachbarsuche. [Messung und Einstellungen im Spiel](raumgruppen.md)

Das Directory bearbeitet geänderte räumliche Beschreibungen. Der Publisher bearbeitet fällige Komponenten auf aktiven Kanten. Auf dem Netzwerkpfad gehen räumliche Metadaten an Slot 1, Entity-Frames unmittelbar an die interessierten Worker. Routenepochen und bestätigte Sequenzen schützen vor alten Frames nach einem Split oder einem Besitzerwechsel. Ein Subscription-Ende entlädt die Sicht. Das lokale `Pool` verwendet dieselben räumlichen Regeln und stellt seine Nachrichtenliste für kontrollierte Zustellung, Verlust und Umordnung bereit. [Peer-Adapter](../crates/is-mesh/src/network_mesh.rs) · [Publisher](../crates/is-mesh/src/publish.rs)

Der lokale Pool prüft standardmäßig alle 40 Schritte seine Last bei einem Zielbudget von 40 ms pro Worker. Die Mindestverweildauer beträgt 80 Schritte, das Transferbudget 8 MiB. Er misst die Worker-Zeit. Spiele können gemessene Gruppenkosten in `GroupTimings` melden. Ohne diese Angaben teilt die Schätzung die Workerzeit auf seine Gruppen auf. Placement berücksichtigt zusätzlich Speicher, bereits verfügbare Daten und die Häufigkeit benötigter Komponenten. `Pool::overloaded` meldet nicht passend platzierbare Gruppen. `Pool::drain` entleert einen Worker innerhalb der angegebenen Kapazitäten. Der synchrone lokale Pool führt seine Worlds seriell aus. Getrennte Prozesse führen ihre Simulation unabhängig aus. [Placement](../crates/is-mesh/src/placement.rs) · [Pool und Drain](../crates/is-mesh/src/pool.rs)

## Zuständigkeit, Aktionen und Wiederanlauf

Objektidentität, Gruppenrevision, Writer-Inkarnation und Authority-Generation sind getrennte Werte. SQLite im WAL-Modus und `synchronous=FULL` bildet den gemeinsamen Kontroll- und Commitpfad. Jede schreibende Operation prüft den aktuellen Writer, seine Inkarnation, Generation und Ausgangsrevision in derselben Schreibtransaktion. Ein veralteter Prozess kann lokal weiterrechnen. Der Speicher weist seine verbindlichen Änderungen zurück. Eine andere Gruppe darf nur mit ihrem eigenen Writer oder einer Genehmigung dieses Writers für exakt denselben wirtschaftlichen Vorgang geändert werden. [Speicher und Transaktionen](../crates/is-mesh/src/store.rs) · [SQLite-Isolation](https://www.sqlite.org/isolation.html)

Ein Transportbatch enthält bis zu 32 geordnete Absichten einer Session und Primärgruppe. Jede Spielaktion gelingt vollständig oder hinterlässt eine dauerhafte Ablehnung. Verschiedene Absichten im Batch können unterschiedliche Ergebnisse haben. Pro Spielaktion sind höchstens 64 Ziele zulässig. Abbau und Bau bündeln Zugriffe und Ableitungen je Chunk. Bestände, Drops, Blockänderungen, Werkzeugzustand, Beleg und Sessionfortschritt werden vor der Erfolgsmeldung gemeinsam gespeichert. Für Handel prüft der Server zusätzlich das Angebot und den Verkäuferbestand. Der zweite Writer genehmigt den Fingerabdruck der Absicht bei seiner lokalen Revision. Verändert sich diese, wartet der Aufrufer auf eine neue Genehmigung. [Inventar und Fachhandler](../examples/infinite-stars/src/lib.rs) · [Absturz zwischen Commit und Antwort](../examples/infinite-stars/tests/process.rs)

Eine logische Session verarbeitet die nächste Sequenz. Ein vorauslaufender Auftrag erhält `Expected`. Die letzten 128 Ergebnisse können unverändert erneut abgeholt werden. Ältere Wiederholungen liefern `Resync` mit dem Wasserstand. Der Client kann bestätigte Revision, Inventar und Aktionsfortschritt neu beziehen. Optimistische Abbau- und Bauänderungen liegen als Overlay vor. Ein vorhergesagter Drop hat dieselbe aus Session, Sequenz und Index abgeleitete ID wie sein bestätigtes Weltobjekt. Pickup und abhängige Vorhersagen werden nach einer Entscheidung neu ausgewertet. Handel zeigt das bestätigte Ergebnis an. [Clientvertrag](../examples/infinite-stars/src/prediction.rs)

Ersetzbare Bewegung verwendet eine eigene Sequenz, einen servergeprüften Zeitstempel und eine vier Ticks lange Eingabe. Doppelte Samples verlängern die Eingabe nicht. Der geordnete Handoff übernimmt Position, Restdauer und verarbeiteten Fortschritt exakt. Im Prozessbetrieb läuft die World mit 20 Hz. Gewöhnliche kontinuierliche Änderungen werden alle 20 Updates gespeichert, neu angelegte und entfernte Entities früher. Beim Verlust eines Prozesses kann Bewegung bis zum letzten erfolgreichen Checkpoint zurückspringen. Bereits bestätigte wirtschaftliche Aktionen bleiben erhalten. [Eingabe und Simulation](../examples/infinite-stars/src/movement.rs)

### Übergabe

1. Die Quelle eröffnet ein Ticket für die bestehende Generation und eine konkrete Ziel-Inkarnation. Sie simuliert während des Vorladens weiter.
   Liegt eine bestätigte Aktion bereits im Store und wird erst im nächsten ECS-Tick sichtbar, melden `Preview` und `Freeze` den Zustand `Preparing`. Der mitgelieferte Controller wartet mit demselben Ticket auf die passende Revision. Erst dann darf er die Gruppe einfrieren.
2. Das Ziel gleicht Datenmanifeste ab. Gleicher geprüfter Inhalt benötigt keine Bulk-Nutzlast. Eine geeignete ältere Basis erlaubt ein Delta. Fehlende Historie führt zum Snapshot. Die Voxelbasis enthält Seed, Generator, Konfiguration, Registry und Inhaltsversion. Das zweite Datenformat ist ein unveränderlicher Bauplan.
3. Das Ziel prüft Inhalt, Pflichtreferenzen und Rekonstruktion im Staging. Änderungen der Gruppe, ihres Inhalts und ihrer Mitglieder fließen vor dem Umschalten ein. Für die lokale Vorbereitung sind vier Nachführungsrunden vorgesehen. Ein erschöpftes Budget liefert Backpressure.
4. Der Speicher friert genau die erwartete Revision ein. Der endgültige Stand enthält kanonische Migrationsfelder, Löschinformationen, erforderliche Daten und Aktionsfortschritt. Das Ziel quittiert dessen SHA-256. Eine frühe Quittung für einen anderen Stand schlägt fehl.
5. Eine atomare Entscheidung setzt neuen Owner und Generation und sichert den exakten persistenten Stand. Erst danach aktiviert das Ziel seine vorbereiteten Entities. Alte Writer bleiben gesperrt. Vorbereitung und Pause werden separat gemessen.

Nach einem Verbindungsabbruch denselben `migrate`-Befehl wiederholen. Die CLI bewahrt das Ticket in `controller.sqlite` im Laufverzeichnis auf. Ein bestätigter Abbruch ist über `cargo run -p infinite-stars -- abort target/stars-net 2` möglich. Der Speicher kann dabei auch eine bereits erfolgte Umschaltung melden. Ein neu gestarteter Worker übernimmt die noch seinem Slot gehörenden Gruppen mit einer neuen Generation. Ein fehlender Spielertransfer im alten Browserpfad führt ebenfalls zum erneuten Claim derselben ID. [Zustandsautomat](../crates/is-mesh/src/handoff.rs) · [Netzwerkprotokoll für die Übergabe](../crates/is-mesh/src/remote.rs)

## Wire-Format und Grenzen

Der Standardadapter verwendet TCP mit gegenseitigem TLS und an Spieler oder Worker-Inkarnation gebundene Zertifikate. Ein Frame beginnt mit `ZZM1`, der Version als Little-Endian-u16 und der Nutzdatenlänge als Little-Endian-u32. Wire-Version 2 ergänzt den räumlichen Zieltyp und die persönliche Spielersicht. Sie verwendet Bincode mit Little Endian und Varints. Authority, Worker und native Clients gemeinsam auf Version 2 aktualisieren. Bestehende Weltkoordinaten und gespeicherte Spielkomponenten verwenden weiter ihren bisherigen Codec. [Transport](../crates/is-mesh/src/wire.rs) · [Client-Sicht](spielersicht.md)

Das Clientverzeichnis kann den erwarteten TLS-Namen in `server-name` vorgeben. Bei älteren lokalen Installationen mit einem ausschließlich für `localhost` ausgestellten Serverzertifikat dort `localhost` eintragen. Die getrennt bereitgestellten Worker verwenden `game.mesh.internal`.

Ersetzbare Frames enthalten Subscription-Epoche, Generation, Revision, Sequenz und bestätigte Basis. Bleibt die Basis unbestätigt, sendet der Publisher einen vollständigen Stand der angeforderten Sicht. Inventar ist privat und exakt, entfernte Clientpositionen können je Achse auf 100 mm quantisiert werden. Workerabhängigkeiten und Handoff erhalten die vom Spiel angegebene Genauigkeit. Die Simulationsfrequenz ist separat konfiguriert. Voxel-Patches vergleichen sortierte Indizes, zusammenhängende Bereiche und Paletten mit Änderungsmaske. Das niederwertigste Bit steht für die erste Zelle beziehungsweise das erste Bit eines Palettensymbols. [Voxelcodec](../crates/is-mesh/src/voxel_wire.rs)

Die Grenzen des Adapters betragen 1 MiB pro Frame, 128 KiB pro Feld und 128 Ziele pro allgemeinem Änderungssatz. Bulk-Daten verwenden Blöcke bis 32 KiB, SHA-256 und einen bestätigten zusammenhängenden Offset. Der Standardcache fasst 64 MiB und bis zu 4.096 Deskriptoren. Aktive oder vorbereitete Daten können gepinnt werden. Der Voxelprovider hält acht historische Stände. Der lokale `serve`-Listener nimmt bis zu 16 gleichzeitige Verbindungen an. Die getrennt bereitgestellten Worker und der Authority-Dienst begrenzen ihren Listener jeweils auf 256 Verbindungen einschließlich Peer- und Verwaltungsverkehr. Kontroll- und Peer-Daten benutzen eigene Hintergrundkanäle. Der Ereignisloop begrenzt die Requests je Session und verarbeitet sie reihum mit einem weichen Zeitbudget. Bereits begonnene atomare Batches laufen zu Ende. Große Provider-Daten werden in Hintergrundjobs oder auf den Netzwerkthreads geprüft. Der ECS-Hook `mesh_rebuild` installiert die vorbereiteten Ableitungen und muss in das Tickbudget passen. [Bulk-Daten](../crates/is-mesh/src/bulk.rs) · [Provider](../examples/infinite-stars/src/data.rs)

`Store::compact` entfernt alte Löschkörper und Gruppenabgänge bis zur bestätigten Revision. Ein Mindeststand erzwingt anschließend einen vollständigen Neuimport älterer Kopien. Eine kleine dauerhafte Namenszeile pro Objekt verhindert die Wiederverwendung seiner ID. Abgeschlossene Handoff-Protokolle haben eine begrenzte Historie. Die Größe dieser Identitätsliste und die Anzahl logischer Sessions wachsen mit der Spielwelt. Der Authority-Dienst erlaubt getrennte Worker-Rechner. Er benötigt einen erreichbaren, intakten Datenträger. Die Betriebsanleitung beschreibt Zertifikate, Wiederanlauf und Backup. [Verteilte Bereitstellung](produktionsabnahme.md)

## Prüfung und Ausgangsstand

```sh
npm run setup
npm test
npm run smoke
cargo test --locked -p is-mesh -p infinite-stars
cargo run --release --locked -p infinite-stars --bin mesh-bench -- target/stars-benchmark.json
npm run bench -- --servers 3 --npcs 3000 --seconds 15
```

Die Workspace-Prüfung umfasst die Browser-Demo, gemeinsame Protokolle, den ursprünglichen Simulationskern und die neuen Contracts. Mehrprozesstests starten ausschließlich eigene Kindprozesse auf vom Betriebssystem vergebenen Ports und verwenden frische SQLite-Dateien. Der Test nach dem wirtschaftlichen Commit beendet den Prozess vor ECS-Anwendung und Antwort. Andere Tests frieren an jeder Transferphase ein, öffnen den Speicher neu, ändern Gruppenmitgliedschaft während der Vorbereitung, prüfen Kompaktierung, langsamen Transfer, alte Quittungen und räumliche Abfragen gegen ein vollständiges Referenzverfahren. [Spiel- und Recovery-Tests](../examples/infinite-stars/tests/contract.rs) · [Prozesstests](../examples/infinite-stars/tests/process.rs) · [Native CI](../.github/workflows/native-mesh.yml)

Der Umbau begann auf `a2c478bf`, dem lokalen Nachfolger des Referenzstands `3ef0d789`. Vor Änderungsbeginn bestanden 642 Rust-Tests, 83 TypeScript-Prüfungen und der Vite-Produktionsbuild. Der alte Sim-Server bietet jetzt `DemoWorker::step` denselben ECS-Schedule für direkte Tests und seine Socket-Schleife an. Die Crates verwenden die Bevy-0.15-Typen. Die bestehenden Referenzmessungen betreffen die ursprüngliche Browser-Demo. [Demo-Baseline](benchmark.md) · [Messungen der Bibliothek](infinite-stars-benchmarks.md)
