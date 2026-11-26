# InfiniteStars Server Meshing
Codex-Arbeitsauftrag für die erste Produktionsreife
Zielmodul: is-mesh · Rust-Import: is_mesh · Bevy: 0.15 gemäß Projektvorgabe

Dieser Auftrag konsolidiert die bisherigen Mesh-Aufträge und ersetzt deren widersprüchliche Umfangsvorgaben. Bereits funktionierende und begonnene Arbeit bleibt erhalten. Die folgende Liste beschreibt den zu prüfenden Zielzustand; sie behauptet nicht, dass diese Funktionen im aktuellen Repository fehlen. Der bisherige Entwicklungsbericht ist ein Ausgangspunkt für die Prüfung, kein Ersatz für Code- und Testnachweise.
## 1. Auftrag und Abschlussziel
Finalisiere die bestehende Server-Meshing-Bibliothek als eigenständig nutzbares, idiomatisches und sehr effizientes Bevy-Modul. Ein Spiel muss eigene Entities und Components registrieren und mit gewöhnlichen Bevy-Systemen simulieren können. Das Mesh übernimmt deren räumliche Organisation, zuständige Worker, Authority-Wechsel, relevante Zustandsverteilung und Lastverteilung.

Damit müssen Spielerfiguren, Raumschiffe, Planeten, Asteroiden, Chunks und weitere Spielobjekte synchronisierbar sein. Diese Objekttypen und ihre Regeln gehören zum Spiel. Das Mesh verarbeitet deren registrierte Identitäten, Positionen, Ausdehnungen, Referenzen, Components und Datenrevisionen.

Abschlussziel ist ein freigabefähiges Mesh-Modul mit dokumentierten Betriebsgrenzen. Die spätere Produktionskapazität eines vollständigen MMO wird separat mit Authentifizierung, realem Client, Gameplay und Produktionshardware abgenommen.

Kein Neustart, kein pauschaler Rollback, kein vollständiger Rewrite und keine erneute grundlegende Architekturphase. Ergänze nur nachgewiesene Lücken.
## 2. Benennung und Zuständigkeiten
Die Produktbezeichnung lautet InfiniteStars Server Meshing. Neue öffentliche Bezeichnungen verwenden InfiniteStars beziehungsweise IS; die Crate heißt is-mesh. Spätere separate Crates heißen beispielsweise is-session und is-economy.

Migriere vorhandene ZZ-Bezeichnungen kontrolliert. Aktualisiere betroffene Imports, Beispiele, Buildskripte und Dokumentation. Bewahre erforderliche Kompatibilität. Ein Namenswechsel darf gespeicherte Objekt-IDs, Schema-IDs, Netzwerkformate oder Checkpoints nicht unbeabsichtigt verändern. Nicht alle Module müssen dafür neue Crates werden.
Bereich	Zuständig
Globale Objektidentität, Spatial Islands, Authority, Worker-Routing	is-mesh
Individuelle Interest Sets, Projektionen, Snapshots, Deltas, Replikationswiederaufnahme	is-mesh
Worker-Kommunikation, Lastverteilung, CPU-Budgetierung, Mesh-Metriken	is-mesh
Technische Migrationszustände, Checkpoint- und Restore-Schnittstellen	is-mesh; Speicherung über vorhandene oder externe Adapter
Login, Passwörter, Account-Tokens, Aeronet-Spielverbindungen, Verbindungszustände und Client-Reconnect	Separates is-session; geplante Anbieter: Ory Kratos und Aeronet
Items, Inventare, Handel, Geld, Eigentum und dauerhafte fachliche Buchungen	Eigenständige Fach-Crate beziehungsweise spezialisierte externe Systeme
Voxelinhalt, Chunk-Generierung, Geometrie-Meshing, Physik, Kampf, Maschinen und KI	Gameplay- und weitere spezialisierte Crates
Langfristige fachliche Weltpersistenz und Betrieb externer Dienste	Separate Persistenzintegration und Deployment


Vorhandene fachliche Beispiele dürfen erhalten bleiben. Sie dürfen keine Pflichtabhängigkeit der allgemeinen Mesh-Runtime sein. Dieser Auftrag verlangt weder ein fertiges Login-System noch ein Wirtschaftsbackend, eine Voxel-Engine oder eine neue Hochverfügbarkeitsplattform.
## 3. Arbeitsweise am bestehenden Repository
1. Aktuellen Code, uncommittete Änderungen, Tests und Dokumentation prüfen; vorhandene Arbeit erhalten.
2. Jede Abnahmeforderung unten einem vorhandenen öffentlichen API-Einstieg und einem Nachweis zuordnen.
3. Nur tatsächliche Lücken als Aufgaben aufnehmen. Vorhandene Tests erweitern, soweit ihr Nachweis nicht ausreicht.
4. Änderungen in überschaubaren, fachlich zusammenhängenden Schritten umsetzen. Umbenennung und Verhaltensänderungen soweit sinnvoll getrennt halten.
5. Bestehende Integrationsbeispiele, Demos, Messwerkzeuge und funktionierende Garantien erhalten.
Die berichteten Tests mit 10.000 simulierten Spielern und 10.000 WebSocket-Verbindungen bleiben historische Vergleichsgrundlagen für ihre jeweiligen Workloads. Sie sind keine automatische Freigabe eines anderen Transports oder eines vollständigen MMO.

Keine neuen Frameworks, Scheduler, Datenbanken oder Protokollimplementierungen nur zur Erfüllung dieses Dokuments einführen. Geeignete vorhandene Mechanismen wiederverwenden.
## 4. Verbindlicher Funktionsumfang des Meshes

### 4.1 Globale Objekte und Referenzen
Globale Objektidentitäten müssen bei Worker-Wechseln und unterstützten Restores erhalten bleiben. Lokale Bevy-`Entity`-Handles sind ausschließlich innerhalb ihrer jeweiligen World gültige Laufzeitreferenzen. Übertragene oder gespeicherte Beziehungen verwenden stabile globale IDs und werden lokal aufgelöst.

Das muss beispielsweise ein Schiff mit zugehörigen Chunks, einer lokalen Bezugsebene und einer steuernden Figur abbilden können. Fehlende oder noch nicht geladene Referenzen brauchen einen definierten Zustand. Objekt- und Component-Entfernung müssen übertragen werden; verspätete Zustände dürfen entfernte Objekte nicht dauerhaft wieder erscheinen lassen.

Keine Pflicht zu einer Entity pro Voxel. Ein großer Planet darf aus getrennt verwalteten Bereichen und externen Datensätzen bestehen. Umfang und Granularität definiert das Spiel über die Registrierungs- und Datenquellenschnittstellen.

### 4.2 Räumliche Organisation
Vorhandene 64-Bit-Millimeterpositionen, lokale Bezugssysteme, Octrees und dynamische Inseln erhalten. Unterstütze registrierte räumliche Ausdehnungen, damit große Objekte nicht ausschließlich über ihren Mittelpunkt sichtbar werden.

Spatial Island, Authority-Gruppe und Worker bleiben unterschiedliche Konzepte. Eine Insel darf mehrere Worker umfassen; ein Worker darf mehrere weit entfernte Inseln bedienen. Verschmelzung oder Trennung einer Insel darf keine unnötige vollständige Neuverteilung erzwingen.

Die vorhandenen Sichtweiten müssen konfigurierbare Spielregeln bleiben. Prüfe Grenzfälle an Sicht-, Insel- und Workergrenzen sowie bei sehr großen Koordinaten.

Für Vergleiche zwischen Bezugssystemen benötigt das Mesh einen dokumentierten Vertrag für die Umrechnung in einen gemeinsamen Raum. Die konkrete Umrechnung kann die Spielintegration liefern. Fehlende oder veraltete Umrechnungsdaten müssen definiert behandelt werden. Dieser Auftrag verlangt keine neue astronomische oder physikalische Transformationsengine.

### 4.3 Authority, Migration und Routing
Für jede definierte schreibende Authority-Einheit darf höchstens eine gültige Authority existieren. Veraltete Generationen dürfen keine akzeptierten autoritativen Zustände, Eingaben oder Mutationen erzeugen. Diese Prüfung muss an den relevanten Verarbeitungs- und Empfangsstellen durchgesetzt werden; eine Marker-Component allein reicht nicht aus.

Bestehendes Vorladen, Snapshot-Transfer, Nachführung und Handoff weiterverwenden. Bei Migration müssen alle registrierten relevanten Components, Referenzen, Revisionen und Steuerungsbindungen konsistent übernommen oder nach ihrem Vertrag neu aufgebaut werden.

Routing folgt der aktuellen Authority anhand globaler IDs. Netzwerk-Endpunkt und Simulationsworker dürfen verschieden sein. Bei unsicherer Authority ist eine kontrollierte Unterbrechung zulässig; zwei gültige Schreiber sind es nicht.

Worker-Ausfall, unterbrochene Übergabe, verzögerte Nachrichten, Wiederanlauf und Netzwerkpartitionen gegen diese Garantie testen. Bestehende konsistente Koordination erhalten. Keinen eigenen Consensus-Algorithmus entwickeln.

### 4.4 Relevante und berechtigte Zustandsverteilung
Worker-Replikation und individuelle Subscriber-Sicht getrennt behandeln. Ein Subscriber bekommt die räumlich relevanten und für ihn freigegebenen Projektionen. Private oder interne Components werden ohne ausdrückliche Freigabe nicht an öffentliche Subscriber ausgegeben.

Vorhandene individuelle Sichtmengen mit einer transportunabhängigen Ausgabe verbinden: Initialzustände, Eintritt, Austritt, Änderungen, Component-Entfernung und Objektentfernung. Ein Adapter muss diese Änderungen ohne ständige vollständige Nachbarschaftsabfrage zustellen können.

Registrierung und Verarbeitung müssen unterscheiden zwischen räumlicher Relevanz, Peer-Projektion, Subscriber-Projektion, Migrationszustand und Checkpoint-Relevanz. Änderungen aktivieren nur notwendige Pfade. Eine Änderung ausschließlich privater Daten darf keine Projektion unveränderter öffentlicher Daten auslösen. Berechnet das Gameplay daraus tatsächlich geänderte öffentliche Zustände, werden diese regulär repliziert. Normale dokumentierte ECS-Mutationen und Entfernen von Components müssen zuverlässig erfasst werden.

Gleiche öffentliche Kodierungen wiederverwenden. Private Payloads dürfen nicht versehentlich in gemeinsam verwendete öffentliche Daten gelangen. Subscriber-bezogene Daten nach einem Rechteentzug vor weiterer Ausgabe erneut berücksichtigen.

Prioritäten, Updatefrequenzen und Budgets konfigurierbar machen. Ein langsamer Subscriber darf andere Subscriber nicht blockieren. Aktuellere Positionszustände dürfen vor dem Versand ältere ersetzbare Zustände verdrängen; notwendige Lifecycle-Informationen brauchen verlässliche Zustellung oder einen definierten Resynchronisierungspfad.

### 4.5 Große Daten und Replikationswiederaufnahme
Vorhandene Mechanismen für Chunks, Baupläne und große Daten erhalten: versionierte Referenzen, Vorladen, Cache-Wiederverwendung, Prüfsummen, Deltas und Fortsetzung unterbrochener Übertragungen.

Kleine zeitkritische Zustände und große Payloads benötigen getrennte Budgets. Ein großer Chunk-Transfer darf Bewegungs- und Lifecycle-Nachrichten nicht unbegrenzt verzögern. Ein vollständiger Planet wird nicht bei jeder Änderung neu übertragen.

Replikationsbaselines und ihre Bestätigungen eindeutig definieren. Eine Transportbestätigung ist nicht automatisch die Bestätigung eines angewandten Weltzustands. Nach Unterbrechungen entweder eine noch gültige Baseline fortsetzen oder einen vollständigen passenden Snapshot senden. Speicher für Baselines und ausstehende Daten begrenzen.

Das Mesh setzt Replikation fort; is-session stellt die Verbindung wieder her. Resume-Daten ersetzen keine neue oder fortbestehende Zugangsberechtigung. Alte Gameplay-Eingaben werden beim Resume nicht erneut abgespielt.
## 5. Schnittstellen für separate Crates

### 5.1 Vertrag mit is-session
is-session übernimmt Kratos, Passwort-/Token-Prüfung, öffentliche Aeronet-Verbindungen und deren Zustandsmaschine. Das Mesh erhält ausschließlich einen serverseitig verifizierten Berechtigungskontext.

Die öffentliche Mesh-API muss ermöglichen:
- Verifizierten Teilnehmer an ein globales Objekt binden.
- Steuerungsberechtigung aktivieren, erneuern, suspendieren und widerrufen.
- Eingaben mit vertrauenswürdig zugeordnetem Absender an die aktuelle Authority routen.
- Berechtigte Subscriptions anlegen, ändern, beenden und wiederaufnehmen.
- Zustandsausgaben und Lifecycle-Ergebnisse an den externen Adapter übergeben.
Die Verantwortung für exklusive Steuerungsrechte muss eindeutig festgelegt sein. is-session entscheidet, ob eine Identität ein Objekt übernehmen darf. Das Mesh übernimmt die technische Bindung über einen konsistenten bestehenden Authority-Pfad und setzt die aktuelle Steuerungsgeneration am ausführenden Worker durch. Gleichwertige vorhandene Mechanismen erhalten; keine voneinander unabhängigen Generationenzähler auf mehreren Gateways.

Unterscheide mindestens Objektidentität, logische Steuerungsbindung, konkrete Verbindung, Subscriber, Control-Epoch und Authority-Epoch. Eine neue Verbindung darf weder alte Eingaben legitimieren noch durch einen verspäteten alten Disconnect deaktiviert werden.

Ablauf oder angewandter Widerruf sperren weitere Eingaben und neutralisieren gehaltene Steuerungswerte vor weiterer Verarbeitung. Verteilte Zustellung eines Widerrufs besitzt eine dokumentierte Verzögerungsgrenze beziehungsweise Ablaufregel; sofortige globale Erkennung eines Netzwerkausfalls wird nicht behauptet.

Passwörter und Kratos-Tokens gehören nicht in Mesh-Components, Logs oder Checkpoints. Öffentliche Client-Nachrichten dürfen die privilegierte Zulassungs-API nicht direkt aufrufen. Die physische Figur bleibt bei Disconnect entsprechend den Spielregeln bestehen.

### 5.2 Vertrag mit Wirtschaft, Items und Persistenz
Fachliche Crates erhalten stabile Objektkennungen, einen dokumentierten Weg für asynchrone Ergebnisse und Routing zur aktuellen Authority. Sie liefern freigegebene Zustandsrevisionen zurück. Das Mesh berechnet keine Preise, Bestände, Rezepte oder Transaktionen.

Ergebnisse müssen nach Migration weiterhin dem Objekt zugeordnet werden können. Bereits extern bestätigte Ergebnisse dürfen nicht allein wegen einer alten anfragenden Authority verlorengehen. Alte Revisionen überschreiben keine neueren; Duplikate werden gemäß dem Integrationsvertrag behandelt. Dauerhafte Deduplizierung und wirtschaftliche Atomarität garantiert das zuständige externe System.

Extern verwaltete Zustände besitzen eine maßgebliche Quelle. Nach Restore darf ein alter Mesh-Checkpoint diese nicht überschreiben. Biete dafür einen Wiederherstellungs-/Abgleichpunkt an, statt selbst eine Wirtschafts-Recovery zu implementieren.

### 5.3 Transport und Codec
Ingress und Egress bleiben über öffentliche Mesh-Schnittstellen nutzbar. Bestehende Vorgaben zu QUIC, bidirektionalen Streams und FlatBuffers sind in den jeweiligen Netzwerkadaptern zu berücksichtigen. Die ECS-API darf keine Kenntnisse konkreter Sockets oder FlatBuffers-Nachrichtentypen in jedem Gameplay-System voraussetzen.

Die bestehende interne Worker-Kommunikation erhalten, soweit sie die Abnahme erfüllt. Öffentliche Client-Verbindungen werden später von is-session bereitgestellt. Daher keinen zweiten großen öffentlichen mTLS-Spieltransport ausbauen. Vorhandene mTLS- und WebSocket-Beispiele bleiben nutzbare Referenzen.

Falls alte globale Verbindungsgrenzen oder Queues die neue externe Schnittstelle begrenzen, diese gezielt entkoppeln. Öffentliche Subscriber dürfen die Ressourcen für Worker- und Verwaltungskommunikation nicht aufbrauchen.
## 6. SQL, technische Speicherung und Fehlerverhalten
Gewöhnliche Bewegung, räumliche Verarbeitung, Input-Routing und Replikation dürfen keine verpflichtenden SQL-Abfragen pro Änderung oder Tick verursachen. Lokale Simulation muss ohne SQL-Backend ausführbar sein.

Trenne bestehende SQLite-Nutzung nach Authority-Metadaten, technischen Checkpoints und fachlichen Transaktionen. Optionale Adapter und Beispiele dürfen bestehen bleiben. Entferne keine funktionierenden Fencing- oder Recovery-Garantien, um formal eine Datenbankabhängigkeit loszuwerden. Konsistente Authority-Koordination und langfristige Speicherung sind von der normalen ECS-Simulation getrennte Aufgaben.

Für technische Zustände sind registrierbare Migrations-/Checkpoint-Daten, klar abgegrenzte Snapshot-Versionen, Integritätsprüfung, asynchrone Storage-Anbindung und ein getesteter Wiederanlauf erforderlich. Nicht persistente Verbindungen und Caches werden nach ihrem Vertrag neu aufgebaut. Aktive Steuerungsrechte dürfen beim Restore nicht ungeprüft wieder gültig werden.

Für V1 darf ein Koordinationsausfall zu einer kontrollierten Unterbrechung führen. Beschreibe unterstützte Fehlerfälle, Wiederanlaufweg, möglichen Verlust seit dem letzten Checkpoint und Voraussetzungen für Datenträger-Recovery. Automatische Übernahme nach Ausfall einer ganzen Maschine ist nur dann eine Zusage, wenn sie mit dem gewählten Backend tatsächlich nachgewiesen wurde.

Die Entwicklung einer neuen Datenbank, eines eigenen Consensus-Systems oder einer allgemeinen hochverfügbaren Storage-Plattform gehört nicht zu diesem Auftrag.
## 7. Idiomatisches ECS und nachweisbare Performance

### 7.1 Öffentliche ECS-Verwendung
Components, Query-Filter, Plugins und SystemSets bilden die öffentliche Laufzeitschnittstelle. Das Spiel soll autoritative Objekte abfragen und eigene Systeme registrieren können. Interne Indizes, Netzwerkhandles und Lookup-Tabellen sind zulässig; sie dürfen keinen widersprüchlichen zweiten fachlichen Objektzustand bilden.

Dokumentiere und erzwinge die Reihenfolge für Netzwerk-/Adaptereingang, Rechte- und Authority-Änderungen, erforderliche Deferred-Command-Anwendung, Input-Prüfung, Simulation und Ausgabe. Ein erkannter Widerruf darf nicht durch eine später angewandte Component-Änderung noch zusätzliche gepufferte Aktionen zulassen.

GameplayReady und der Login-Lifecycle gehören zur Session-/Spielintegration; die meshseitige Authority und Steuerungsfreigabe müssen damit über öffentliche APIs kombinierbar sein. Keine globale Bevy-State-Machine für alle einzelnen Spieler erzwingen.

### 7.2 Keine unnötige Arbeit
Kontinuierliche Berechnungen dürfen dichte Queries verwenden. Diskrete Änderungen und Anfragen sollen nur die betroffenen Objekte verarbeiten. Vermeide unnötige vollständige Weltprojektionen, Re-Kodierung unveränderter Payloads und globale Synchronisierung unabhängiger Gruppen.

Den bereits gemeldeten Benchmark privater Änderungen erneut prüfen und als Regressionstest verwenden. Ohne Änderungen öffentlicher Daten müssen öffentliche Projektionen bei ausschließlich privaten Änderungen ausbleiben; notwendige Checkpoint- und Migrationsarbeit wird getrennt gemessen. Keine willkürliche Zeitvorgabe aus einem anderen Rechner übernehmen.

### 7.3 Ganze Worker-Last messen
Erfasse die vollständige Tickdauer und relevante Arbeit außerhalb des Bevy-Simulationsabschnitts: Input-Verarbeitung, Routing, Projektionen, Serialisierung, Peer-/Subscriber-Ausgabe, Checkpoint-Erstellung, Migration und Wartezeiten.

Zusätzlich erfassen: Warteschlangenalter, ausstehende Bytes, Prozessspeicher, CPU-Last und Deadline-Verletzungen. Gruppenbezogene Last entweder ausreichend messen oder sichtbar als Schätzung kennzeichnen. Der Lastverteiler darf zusätzliche Arbeit nach dem Bevy-Tick nicht übersehen. Parallele, überlappende Phasen nicht als serielle Gesamtdauer addieren.

### 7.4 Threads und Worker müssen messbar helfen
Die Entwicklungsmaschine hat 20 logische CPU-Threads. Es gibt keine lokale Pflichtabnahme mit 10.000 echten Spielern. Spätere EPYC-Kapazität wird auf repräsentativer Hardware gemessen.

Ein gemeinsames nachvollziehbares CPU-Budget muss Simulation, Worker-Ausführung, Bevy-Taskpools, I/O und Hintergrundarbeit berücksichtigen. Mehr Worker-Welten bedeuten nicht automatisch mehr parallel arbeitende Threads. Keine unbeschränkten Threadpools pro Worker und keine unkontrollierte verschachtelte Parallelisierung.

Nutze die vorhandene Benchmark-Infrastruktur für:
- Strong Scaling: gleiche Arbeitslast mit unterschiedlichen CPU-Budgets, beispielsweise 1/2/4/8 Simulationsthreads, bei ausreichender Arbeitsmenge.
- Weak Scaling: zusätzliche unabhängige Gruppen und Objekte proportional zu zusätzlichen Ressourcen; Tickdauer und Durchsatz beobachten.
- Worker-Topologien: bei gleichem Gesamtbudget unterschiedliche Worker-/Prozessverteilungen vergleichen.
- Kontrastierende Lasten: räumlich getrennte Gruppen, dichte Sichtkontakte, große Datenübertragungen und eine einzelne teure Authority-Gruppe.
Lastgenerator, Betriebssystem und Hintergrundarbeit im Budget berücksichtigen. Physische Kerne, logische Threads und ungleich leistungsfähige Kerne getrennt dokumentieren. Kleine Arbeitsmengen dürfen seriell schneller bleiben. Keine lineare EPYC-Hochrechnung aus Laptopwerten.

Die freigegebene Konfiguration muss bei geeigneter unabhängiger Last einen reproduzierbaren Nutzen zusätzlicher Ressourcen zeigen. Dokumentiere verbleibende Engpässe. NUMA- und Affinity-Optimierungen bleiben spätere, messungsgetriebene Optionen; dafür wird jetzt kein neues Subsystem verlangt.

### 7.5 Begrenzte Ressourcen
Queues, Nachrichtengrößen, ausstehende zuverlässige Daten, Baselines, Blob-Reassembly, Caches und parallele Migrationen erhalten konfigurierbare Grenzen sowie definiertes Überlastverhalten. Ersatz älterer Zustände, Zurückweisung, Resynchronisierung oder Trennung des betroffenen Adapters müssen bewusst gewählt sein.

Ein fehlerhafter oder langsamer Teilnehmer darf keinen unbegrenzten Speicherverbrauch verursachen. Interne Worker- und Verwaltungskommunikation benötigt geschützte Budgets. Die interne Vertrauensgrenze und Berechtigungen für privilegierte Mesh-Operationen müssen durchgesetzt werden. Keine eigene Kryptografie entwickeln.
## 8. Umsetzung und Release-Nachweise
Nach der Bestandsaufnahme verbleibende Arbeit in dieser Reihenfolge abschließen; bestehende technische Abhängigkeiten berücksichtigen:
1. Baseline, Benennung und Verantwortlichkeiten klären.
2. Nachgewiesene Authority-, Daten- oder Berechtigungsfehler beheben.
3. Öffentliche Adapterverträge und externes Bevy-Beispiel vollständig nutzbar machen.
4. Notwendige Replikations-, Ressourcen- und SQL-Entkopplungslücken schließen.
5. Messung und Skalierung an den vorhandenen Workloads abnehmen.
6. Dokumentation, Checkliste und Release abschließen.
Ein externes Bevy-Beispiel muss vollständig über öffentliche APIs eigene Typen für Figur, Schiff, Planet/Asteroid und Chunk registrieren können. Vereinfachte Test-Components genügen; es wird keine neue Spielmechanik implementiert. Ein Testadapter demonstriert externe Steuerungsfreigabe, individuelle Sicht, Unterbrechung, erneute Zulassung und Replikationsfortsetzung. Ein kleiner externer Ergebnisadapter demonstriert die Anbindung fachlicher Zustände nach Migration.

Für jeden Prüfpunkt unten dokumentiert Codex: Status, Code-/API-Stelle, Test oder Messprofil, Ergebnis und verbleibende Einschränkung. Zulässige Status sind BESTANDEN, OFFEN, BLOCKIERT oder NICHT ANWENDBAR mit nachvollziehbarer Begründung. Ein verbindliches Kernkriterium darf nicht durch eine unbegründete Nicht-Anwendbarkeit umgangen werden.

Nachweise nennen Git-Stand, Konfiguration und bei Performance Hardware, optimierten Build, Wiederholungen und Messdauer. Vorhandene gültige Tests verwenden; neue Tests auf konkrete Fehler- und Integrationsrisiken konzentrieren. Routineänderungen rechtfertigen keine komplette neue Testsuite.

Eine kompakte Abnahmedatei mit Verweisen auf vorhandene Dokumentation genügt. Keine Vielzahl redundanter Architekturdokumente erzeugen. Nicht ausgeführte Tests bleiben offen.
## 9. Grenze der ersten Produktionsreife
V1 ist freigabefähig, wenn die verbindlichen Mesh-Kriterien unten nachgewiesen sind und der dokumentierte Betriebsmodus sicher verwendet werden kann. Eine zentrale externe Koordination mit kontrolliertem Wiederanlauf kann für diesen Modus zulässig sein; automatische Hochverfügbarkeit darf dann nicht behauptet werden.

Für diese Modulfreigabe werden nicht zusätzlich verlangt: ein fertiges is-session, ein echter Kratos-Betrieb, ein Wirtschaftsbackend, fertige Spielphysik, ein öffentlicher 10.000-Spieler-Test, ein vollständiger EPYC-Verbund oder der fertige InfiniteStars-Client. Ihre späteren Integrations- und Kapazitätsabnahmen bleiben ausdrücklich offen.

Für das geprüfte Profil müssen Tick-/Latenzbudget, zulässige Deadline-Verletzungen und Ressourcenlimits vor der Abschlussmessung festgelegt werden. Ein konfiguriertes 20-Hz-Profil hat beispielsweise 50 ms Tickbudget; daraus folgt keine Zusage für beliebige Spielbelastungen. Die Ergebnisse werden für genau dieses Profil berichtet.

Nach bestandener Abnahme die öffentliche API versionieren und das Modul als Grundlage weiterer Crates verwenden. Zusätzliche Funktionen entstehen anschließend nur aus konkret nachgewiesenen Integrationsanforderungen.
## 10. Abnahmecheckliste für Codex

Der nachfolgende Eingangstext definiert die Kriterien. Status, konkrete Nachweise und Betriebsgrenzen stehen im [verbindlichen Abnahmeprotokoll](../docs/mesh-v1-abnahme.md).
Die Kästchen bleiben bis zum tatsächlichen Repository-Nachweis offen. Alle Kernanforderungen müssen erfüllt sein; externe Betriebsannahmen werden ausdrücklich angegeben.
- M01 – Name und Kompatibilität. InfiniteStars/`IS`, is-mesh und is_mesh sind konsistent; betroffene Builds und Beispiele funktionieren. IDs, gespeicherte Schemas und Protokolle bleiben kompatibel oder besitzen einen dokumentierten, geprüften Migrationsweg.
- M02 – Eigenständige Bibliothek. Ein externes Bevy-0.15-Projekt bindet eigene Objekttypen und Systeme ausschließlich über öffentliche APIs ein. Login, Wirtschaft, Physik und öffentlicher Spieltransport sind keine Pflichtimplementierungen des Mesh-Cores.
- M03 – Objektarten und große Strukturen. Figur, Schiff, Planet/Asteroid und Chunk sind als externe Testtypen synchronisierbar. Große Strukturen können in getrennten Objekten und versionierten Datensätzen organisiert werden; keine Pflicht zu einer Entity pro Voxel.
- M04 – Stabile Identität und Referenzen. Globale IDs und Objektbeziehungen bleiben bei Migration und unterstütztem Restore erhalten. Test: Schiff, Chunk und Figur wechseln Worker; Referenzen zeigen auf die richtigen neuen lokalen Entities.
- M05 – Räumliche Korrektheit. Sicht an Grenzen, große Koordinaten, räumliche Ausdehnungen und lokale Bezugssysteme sind getestet. Der Umrechnungsvertrag für gemeinsame Räume und das Verhalten bei fehlenden Daten sind dokumentiert.
- M06 – Inseln und Zuständigkeiten. Eine Insel kann mehrere Worker umfassen; ein Worker kann mehrere entfernte Inseln bedienen. Verschmelzung, Trennung und Objektwechsel erhalten Sicht und Authority korrekt.
- M07 – Exklusive Authority. Nach verzögerten Nachrichten, Partitionen und Worker-Wiederkehr werden alte schreibende Generationen zurückgewiesen. Koordinationsverlust erzeugt keine zweite gültige Authority.
- M08 – Vollständige Migration. Abbrüche vor, während und nach dem Handoff sind getestet. Registrierte Zustände, Referenzen und Revisionen bleiben erhalten oder werden definiert wiederhergestellt; keine doppelte gültige Objekt-Authority.
- M09 – ECS-Reihenfolge. Registrierung, Query-Filter und SystemSets sind öffentlich dokumentiert. Änderungen an Authority und Steuerungsrechten werden vor weiterer Input-Ausführung wirksam; erforderliche Deferred-Barrieren sind getestet.
- M10 – Getrennte Änderungspfade. Private Änderungen ohne öffentliche oder räumliche Folgeänderung erzeugen keine öffentlichen Component-Projektionen. Unveränderte Positionen lösen keine unnötigen räumlichen Updates aus. Component-Entfernung und Despawn werden zuverlässig verteilt.
- M11 – Individuelle und vertrauliche Sicht. Zwei Subscriber am selben Worker erhalten unterschiedliche berechtigte Zustände, auch über Workergrenzen. Private Felder gelangen weder in öffentliche gemeinsame Payloads noch nach Rechteentzug in neue Ausgaben.
- M12 – Zustandsreplikation und Resume. Initialzustand, Deltas, Enter/Leave, Reihenfolge, Duplikate, fehlende Baselines und Snapshot-Fallback sind getestet. Wiederaufnahme erfordert gültige externe Zulassung und spielt keine alten Inputs erneut ab.
- M13 – Große Payloads. Chunk-/Bauplan-Transfer unterstützt die vorgesehenen Versionen, Prüfsummen und Wiederaufnahme. Unterbrechung und beschädigte Daten sind getestet; zeitkritische Ausgaben bleiben innerhalb des festgelegten Testbudgets bedienbar.
- M14 – Externe Session-Zulassung. Ein externer Testadapter bindet, erneuert, suspendiert und widerruft Steuerungsrechte ohne Passwörter oder Account-Tokens im Mesh. Öffentliche Nachrichten können keine privilegierten Grants selbst erzeugen.
- M15 – Alte und konkurrierende Verbindungen. Gleichzeitige Übernahmeversuche, verspätete Inputs und alte Disconnects sind getestet. Der aktuelle Authority-Worker setzt Control-Epoch, Objektbindung und Ablauf durch. Widerrufsverzögerung und Fehlerverhalten sind definiert.
- M16 – Externe Fachsysteme. Ein asynchrones Ergebnis erreicht nach Migration die aktuelle Authority. Wiederholte Ergebnisse und ältere Revisionen werden gemäß Vertrag behandelt; keine wirtschaftliche Transaktionslogik oder dauerhafte Exactly-once-Zusage im Mesh.
- M17 – SQL-armer Simulationspfad. Lokale Simulation läuft ohne SQL-Backend. Gewöhnliche Bewegung, Routing und Replikation erzeugen keine verpflichtenden SQL-Zugriffe pro Änderung oder Tick. Erforderliche Koordinations-/Checkpointzugriffe sind separat ausgewiesen.
- M18 – Technischer Wiederanlauf. Unterstützter Checkpoint-Restore wurde ausgeführt. IDs, Authority-Fencing, Schema-/Integritätsprüfung und Neubindung externer Zustände funktionieren. Alte Steuerungsrechte werden nicht ungeprüft reaktiviert.
- M19 – Begrenzte Ressourcen. Nicht lesende Subscriber, übergroße/fehlerhafte Payloads und volle Queues sind getestet. Speicher bleibt begrenzt; andere Teilnehmer sowie interne Steuerkommunikation bleiben entsprechend ihren Budgets bedienbar.
- M20 – Vollständige Lastmessung. Zusätzliche Arbeit außerhalb der Bevy-Simulation erscheint in Metriken und Lastbewertung. Prozessspeicher, Warteschlangenalter und gruppenbezogene Messung oder gekennzeichnete Schätzung sind verfügbar.
- M21 – Kontrollierte Parallelisierung. Alle relevanten Threadpools und Hintergrundaufgaben sind budgetiert. Strong/Weak Scaling und mehrere Worker-Topologien zeigen für geeignete unabhängige Last reproduzierbaren Mehrdurchsatz. Grenzen sind dokumentiert.
- M22 – Reproduzierbare lokale Abnahme. Optimierte Builds, Testparameter, Git-Stand, p50/p95/p99, Deadline-Verletzungen und Ressourcenwerte sind festgehalten. Getrennte und dichte Sichtprofile sowie ein angemessener Stabilitätslauf sind geprüft; keine lokale 10.000-Client-Pflicht.
- M23 – Betrieb und Release. Start, Konfiguration, kontrolliertes Leeren/Stoppen, unterstützte Fehlerfälle, Restore und Protokoll-/Schemakompatibilität sind dokumentiert. Offene echte-Client-, EPYC-, Mehrrechner- und Hochverfügbarkeitsabnahmen sind klar benannt; die öffentliche API ist versioniert.
