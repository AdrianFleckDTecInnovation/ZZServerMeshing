# is-mesh in ein Bevy-Spiel integrieren

Mit `is-mesh` 1.0 besitzt jeder Worker eine Bevy-0.15-World. Dein Spiel definiert Komponenten, Simulationssysteme und Spielregeln. Die Bibliothek speichert stabile Objekt-IDs, prüft Schreibrechte und übergibt zusammengehörige Entities als Gruppe an einen anderen Worker. [Verbindlicher Funktionsumfang und Nachweise](../../docs/mesh-v1-abnahme.md)

## Vom Checkout zum ersten Spielstand

In Infinite Stars sehen Spieler standardmäßig andere Spieler bis 100 m und Schiffe bis 500 m. Die eigene Sicht bleibt auch innerhalb einer gemeinsamen Insel unabhängig. [Reichweiten und Spielerclient anbinden](../../docs/spielersicht.md)

Für getrennte Rechner zeigt die Betriebsanleitung die Einrichtung von Authority, Worker-Slots, Zertifikaten und Spielerclient. Sie ordnet die Anforderungen den zugehörigen Integrationstests zu. [Betrieb und Abnahme](../../docs/produktionsabnahme.md)

Für die Dimensionierung deines Spiels kannst du aktive Spieler, ruhende Spieler, Worker-Parallelität und dauerhafte Checkpoints getrennt messen. [Tickprofil und Startbefehle](../../docs/tickprofil.md) · [Einsatzaudit mit eigenen ECS-Spielregeln und Freigabekriterien](../../docs/einsatzaudit.md)

1. Lege das Spiel und diesen Checkout nebeneinander. Ergänze im **Spiel** die Abhängigkeiten. Ein bestehendes `bevy = "0.15"` kann die beiden Bevy-Teilcrates ersetzen, wenn du deren Typen aus `bevy::prelude` importierst.

   ```toml
   [dependencies]
   is-mesh = { version = "1", path = "../ZZServerMeshing/crates/is-mesh" }
   bevy_app = "0.15"
   bevy_ecs = "0.15"
   serde = { version = "1", features = ["derive"] }
   ```

2. Übernimm das ausführbare Beispiel als `src/main.rs`. Es registriert eine Komponente, lädt oder erzeugt einen Reaktor, führt einen autoritativen Spielschritt aus und liest seinen gespeicherten Energiewert zurück. [Beispielcode](examples/persistent_game.rs)
3. Führe im Spielverzeichnis **zweimal** `cargo run -- world.sqlite` aus. Eine neue Datei ergibt zuerst `energy=1`, danach `energy=2`. Die Reaktor-ID bleibt gleich. Im Mesh-Checkout lautet derselbe Aufruf `cargo run --locked -p is-mesh --example persistent_game -- target/world.sqlite`. Rust stable und ein C-Linker reichen. Unter Windows benötigst du die MSVC-Toolchain, C++ Build Tools und das Windows SDK.

Für den Bezug aus deinem Fork verwende im Mesh-Checkout den vorbereiteten Branch.

```sh
git switch feat/bevy-game-integration
git fetch origin
git rebase origin/main
git push -u origin feat/bevy-game-integration
git rev-parse HEAD
```

Ersetze im Spiel den lokalen Pfad durch `is-mesh = { git = "https://github.com/AdrianFleckDTecInnovation/ZZServerMeshing.git", rev = "<ausgegebener Commit>" }`. Übernimm die aktualisierte `Cargo.lock` ins Spiel-Repo. Im Mesh-Checkout öffnet `cargo doc --locked -p is-mesh --no-deps --open` die lokale API-Referenz mit dem vollständigen Beispiel.

## Welche Entscheidungen trifft das Spiel?

| Aufgabe | Vertrag mit dem Mesh |
| --- | --- |
| Identität und Zuständigkeit | `Managed::new(object_id, group_id)` verbindet eine dauerhafte ID mit ihrer Übergabegruppe. Speichere Referenzen als `Id`. Ein Bevy-`Entity` gilt nur in der aktuellen World. Lege Objekte mit gegenseitigen Pflichtreferenzen in dieselbe Gruppe. |
| Komponenten | Registriere jeden Typ über `mesh_component` mit fester Schema-ID, Version und Codec. `persist` steuert Checkpoints, `migrate` die Übergabe. `Public` erreicht abonnierende Clients und Worker, `Private` den berechtigten Besitzer, `Workers` abonnierende Worker, `Local` keine Replikation. |
| Spielschritte | Filtere schreibende Systeme mit `With<Simulated>`. Eine `Replica` ist eine entfernte Sicht. `Staged` kennzeichnet vorbereitete Objekte während einer Übergabe. Lokale Ableitungen wie Collider und Bevy-Referenzen baut `mesh_rebuild` aus importierten Komponenten auf. |
| Start und Neustart | Für einen lokalen Store öffne die bisherige Weltdatei, rufe einmal pro Prozess und Slot `start_worker(slot)` auf, installiere `MeshPlugin::persistent` und registriere Codecs und Provider. Lade vor dem ersten Tick jede Gruppe aus `store.owned(slot)?` mit `restore_group(app.world_mut(), group)?`. Das gilt auch für eine leere Gruppe nach einem abgebrochenen Erststart. Erzeuge Startobjekte nur bei `Lookup::Unknown`. `Deleted` bleibt belegt. |
| Bewegung und externe Ergebnisse | `checkpoint_group` bestätigt den technischen Stand. Ein Fachdienst hält seinen eigenen Buchungsstand. Nach einem Umzug löst der Adapter das Objekt erneut auf und übergibt die externe Revision mit `apply_external`. `hydrate_external` lädt den maßgeblichen Fachstand nach Restore. Das persistente Referenzspiel verwendet für eigene Beispielaktionen weiterhin typisierte `mesh_action`-Handler. |
| Spielercluster und große Daten | Melde Objekte als `Presence` und Interessenbereiche als `Area` an die `Directory`. Beim Annähern kommen räumliche Abonnements hinzu, beim Trennen laufen sie aus. Getrennte Inseln benötigen weiterhin Kontrollverkehr. `mesh_provider` und `mesh_datasets` verknüpfen Chunks und Baupläne mit geprüften Datenmanifesten. |

## Spiel und Session anbinden

Ein vollständiges externes Spiel mit eigenen Components lässt sich mit `cargo run --locked -p is-mesh --example session_game` ausführen. `npm run test:game` kompiliert es auch außerhalb des Workspace. Das Beispiel enthält die Registrierung, ein gewöhnliches Bewegungssystem und die Übernahme lokaler Bevy-Referenzen nach dem Wechsel der World. [Vollständiges Spielbeispiel](examples/session_game.rs)

1. **World aufbauen.** Für lokalen Betrieb ohne Datenbank `MeshPlugin::transient()`, `MeshSessionPlugin` und `LocalSubscriberPlugin` installieren. Für dauerhaft koordinierte Worker `MeshPlugin::persistent(store, worker)` verwenden. `SpatialRule` meldet die Ausdehnung, `SightRule` den Bezugspunkt. `SpatialDependencies` enthält die Schema-IDs aller Komponenten, von denen diese Regeln abhängen. Bei Änderungen externer Transformdaten die betroffenen Objekte mit `refresh_spatial` melden.
2. **Eingaben übernehmen.** Der serverseitige Sessionadapter übergibt einen geprüften `SessionGrant` mit Principal, logischer Session, Objekt, Control-Epoch und Ablauf. `admit` bindet die konkrete Verbindung. `SessionIngress::submit` erhält das zugehörige `ControlBinding` und eine steigende Sequenz. Erneuern, Suspendieren, Widerrufen und Schließen verwenden dieselbe Bindung. Für die Übernahme durch eine neue Verbindung muss der vertrauenswürdige Aussteller die Control-Epoch erhöhen.
3. **Simulieren.** Im `PreUpdate` laufen `MeshSystems::Ingress → Authority → Control` mit Deferred-Barrieren. Das Spiel verarbeitet in `Update` nur `ValidatedInput` an Entities mit `Simulated` und `MeshReady`. Eigene fachliche Bereitschaft kann als zusätzlicher Query-Filter hinzukommen. Bei angewandtem Widerruf, Ablauf oder fehlender Koordinationsbestätigung entfernt der Worker die Steuerfreigabe vor dem nächsten Spielschritt. Eine bereits begonnene Simulation kann zu Ende laufen. Uhren der Hosts müssen synchronisiert sein.
4. **Sicht zustellen.** Der Adapter bindet eine `SubscriberId`. Nach der Änderungserfassung in `PostUpdate` kann `MeshSystems::Output` die Ausgabe abholen. Lokal liefert `SubscriberStream::next_local` Ereignisse, im Pool `next`, über mehrere Prozesse `WorkerSubscription`. Erst nach Anwendung des Frames `applied` bestätigen. Das Raster für Worker-Kontakte und die persönliche Reichweite sind getrennt einstellbar. [Spielersicht und Frame-Vertrag](../../docs/spielersicht.md)
5. **Nach Unterbrechung fortsetzen.** Die Sessionintegration entscheidet über den erneuten Zugang. Bei passender Stream-ID und bestätigter Baseline setzt das Mesh mit Deltas fort. Andernfalls verwirft der Adapter bei `Reset` seinen bisherigen Sichtstand und installiert den neuen Snapshot. Eine ersetzte Verbindung kann die aktuelle Bindung weder steuern noch durch einen verspäteten Disconnect schließen.

## Versionen und Betrieb

Die öffentliche Rust-API folgt SemVer. Für reproduzierbare Builds im Spiel den freigegebenen Git-Commit und die eigene `Cargo.lock` festhalten. Der bisherige Paketname `zz-mesh` und Import `zz_mesh` werden über eine Fassade weiter unterstützt. Globale Objekt-IDs, Schema-IDs, Checkpoint-Hashdomäne und bestehende Nachrichten des Worker-Protokolls v3 behalten ihre Bedeutung. Die zusätzliche Operatorabfrage `metrics` benötigt einen aktualisierten Worker. Das technische Store-Format 2 prüft Datensätze per SHA-256. Beim Upgrade alter Formate zuerst eine vollständige Sicherung anlegen. Ein Rückwechsel auf eine Binärdatei für Format 1 benötigt das ursprüngliche Backup. [Kompatibilitätsprobe](../zz-mesh/examples/store_roundtrip.rs) · [Betrieb und Wiederanlauf](../../docs/produktionsabnahme.md)

- **Speicher.** Im lokalen Pool öffnet das Spiel seine SQLite-Welt. Für getrennte Hosts öffnet allein der Authority-Dienst das Welt-Volume. Die Worker verwenden den typisierten Store über gegenseitiges TLS. Für ein kaltes Backup Authority und Verwaltungsbefehle beenden, das vollständige geschützte Kontrollverzeichnis sichern und den Wiederanlauf auf einer Kopie prüfen. [Bereitstellung und Recovery](../../docs/produktionsabnahme.md)
- **Kompatibilität.** Der Store erkennt bestehende Weltdateien aus diesem Projekt und markiert ihr Format beim ersten Öffnen. Ein unbekanntes Format bricht den Start ab. Ein geändertes Komponentenformat benötigt eine geplante Datenmigration. Unbekannte Schema-IDs und inkompatible Versionen verhindern die Aktivierung einer Gruppe.
- **Fehler.** Bei `Fenced` hat der Worker seine Schreibberechtigung oder Revision verloren. Ermittle den aktuellen Owner. Bei `Busy` warte vor einer begrenzten Wiederholung. Ein fehlender Codec oder Provider muss vor dem nächsten Ladeversuch registriert sein. Das Beispiel für getrennte Prozesse nutzt gegenseitiges TLS und eine lokale Entwicklungs-CA. Für den Betrieb stellt der Betreiber Zertifikate und die Zuordnung von Identitäten zu Spielrollen bereit.
- **Nachweis.** `npm run test:game` baut ein separates Spiel aus dem aktuellen Git-Commit und prüft zwei Prozessstarts. `cargo test --locked -p is-mesh -p infinite-stars` prüft Bibliothek und Worker-Prozesse. `npm run bench:quick` misst bewegte, getrennte und verschmelzende Gruppen sowie die Eingabelatenz der Browser-Demo. `npm run bench:cluster-wire` prüft die Sicht der Worker über TCP/mTLS. [Betriebsanleitung](../../docs/infinite-stars.md) · [Spielergrenze und Graphen](../../docs/cluster-performance.md)
