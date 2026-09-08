"""One verdict per thread I opened on MR !591, written as a draft for the poster.

Vlad pushed two commits after the review and wrote nothing on any thread, so
every verdict here is read off the code rather than off a reply. That is the
case this whole chain exists for: silence is not "unaddressed", and a thread
closed on a guess is worse than one left open.

Each body says the same three things in the same order -- what changed, whether
it settles the objection, and what closing or not closing follows from that --
because a reviewer scanning eleven of these should not have to hunt for the
verdict.

Run:  python3 drafts/mr591_verdicts.py > ~/.local/state/spark-review/mr591.draft.json
"""

import json

IID = "591"
SESSION = "mr591-verdicts"

R = []


def reply(did, resolve, body):
    R.append({"discussion_id": did, "resolve": resolve, "body": body.strip()})


# ---------------------------------------------------------------- T1: closed
reply(
    "c1eabde8a9c32b427b6dd387e58ecdfed53a69ac", True,
    """
**Закрываю: исправлено в `16cba0b3c`.**

Все три audit-цикла теперь проходят под условием фактического результата
`DeleteGroupsByIDs`, а не по исходному набору:

- `auditDonorRetirement` (`upsert.go:728`)
- `auditRetirement` (`upsert.go:1137`)
- `auditGhostRetirement` (`upsert.go:1325`)

При недоборе audit **пропускается**, а не пишется по плану — то есть отклонённая
базой строка больше не может произвести запись `group_deleted`, что описание MR
и обещало.

Отдельно отмечу честность комментария в коде: набор «нельзя сузить до реально
ретайрнутых строк — молчаливый след лучше ложного». Это ровно тот выбор, который
я бы и предложил: не знать точнее и не притворяться, что знаешь.
""")

# ------------------------------------------------------------- T2: not closed
reply(
    "c74760dbe9c891eb8466b04e307411e14d1e792c", False,
    """
**Не закрываю: код исправлен, документ — нет, и теперь они противоречат друг другу.**

Что починено. Комментарий в `upsert.go:1037-1042` теперь говорит именно то, чего
не хватало: асимметрия существует «in THIS Go predicate only — not in the
database. Every id this loop routes to `toDelete`, `emptyCanonical` ids included,
goes through `deleteGroupsByIDsSQL` and meets its `NOT EXISTS`». Возражение по
коду снято полностью.

Что осталось. `docs/MATCHING.md:532-537` не тронут ни одним коммитом после ревью
и по-прежнему утверждает обратное:

> **The `emptyCanonical` arm is deliberately NOT guarded, and the asymmetry is
> load-bearing.** … Guarding that arm too would raise 23505 and abort the whole
> cycle transaction.

Первое предложение теперь прямо опровергается комментарием в коде. Второе неверно
самостоятельно: `NOT EXISTS` не поднимает 23505 — он молча отклоняет строку;
23505 приходит позже, из `SetCanonicalEntityIDs`, именно потому что слот остался
занят. Это два разных механизма, и документ склеивает их в один.

Правка узкая — переписать эти два абзаца под формулировку из `upsert.go`. Держу
тред открытым до неё, потому что расхождение кода и документа по вопросу «защищена
ли ветка» — ровно тот класс, из-за которого тред и заводился.
""")

# ------------------------------------------------------------- T3: not closed
reply(
    "3559da819c0e80a38b9c8d4c94304ebc93f8fe7a", False,
    """
**Не закрываю: гарантия «наследования» по-прежнему не выполняется, и формулировка не смягчена.**

`docs/MATCHING.md:500-503` на HEAD не изменён:

> `deleteGroupsByIDsSQL` — **the single soft-delete statement** — carries
> `AND NOT EXISTS (...)`, so **every writer of that statement inherits the
> protection, including writers added later** that never read this document.

На том же HEAD на `entity_match_groups` живут как минимум два незащищённых
soft-delete'а вне этого оператора:

| Место | `delete_reason` | Guard |
|---|---|---|
| `canonical_repair.go` `softDeleteGhostGroup` | `orphan_repair` | `WHERE id = $1 AND deleted_at IS NULL` — по участникам нет |
| `canonical_repair.go:1395` (consolidate loser) | `orphan_repair_consolidate` | то же — по участникам нет |

`git diff c157f20f0..4a09875c8 -- internal/infrastructure/store/canonical_repair.go`
пуст: ни один из них этим MR не тронут.

Претензия не в том, что их надо закрыть в этом MR — скоуп я расширять не прошу.
Претензия в слове «single» и в «inherits by construction»: это утверждение о
структурной гарантии, а гарантии нет — есть один защищённый оператор из трёх.
Достаточно заменить его на «the statement step 5 and `softDeleteDanglingTx` share»
и перечислить незащищённые пути (или завести на них тикет) — тогда закрою.
""")

# ---------------------------------------------------------------- T4: closed
reply(
    "60d92b0f860a2f652078f218a83abcba5e105318", True,
    """
**Закрываю: поведенческий тест появился в `4a09875c8`.**

`TestReviveGroupsByIDsTx_RevivesStrandedAndSparesEmpty` (`matching_test.go:396`)
гоняет `ReviveGroupsByIDsTx` против живого пула на фикстурах stranded / empty /
живой контроль — то есть проверяет поведение guard'а, а не текст запроса, и
падал бы от удаления `AND EXISTS (...)` независимо от того, как переписана строка.

Тест включён в `-run`-фильтр `integration-tests-store`, так что защита от
регрессии есть и в CI, а не только локально. Асимметрия с зеркальным
`NOT EXISTS` устранена — закрываю.
""")

# ------------------------------------------------------------- T5: not closed
reply(
    "a7b23f57ae9e3f7345f7054a49d75ee1d76a110d", False,
    """
**Не закрываю: `MEASUREMENTS.md` не менялся.**

`git diff c157f20f0..4a09875c8 -- docs/tasks/MC-3747/MEASUREMENTS.md` — пусто.
Три разных итога для одной популяции (§1 — 13 363, §2 — 13 373, §10 — 13 366)
стоят там же, и запрос §1 по-прежнему не закоммичен, так что расхождение в 3
строки нечем свести: разные предикаты это или разные моменты замера — из
документа не определить.

Закрывать нечего: возражение целиком про содержание документа, а документ не
тронут. Минимум для закрытия — либо доложить SQL к §1, либо явно пометить, что
цифры сняты в разные моменты, и указать когда.
""")

# ------------------------------------------------------------- T6: not closed
reply(
    "f59b508e681a8c569d291215eff02a385ad0f542", False,
    """
**Не закрываю: `MEASUREMENTS.md` не менялся.**

`git diff c157f20f0..4a09875c8 -- docs/tasks/MC-3747/MEASUREMENTS.md` — пусто.
Критерий приёмки «после прогона `stranded_members` → 0» остался тавтологией:
формула CLI — `SET deleted_at = NULL WHERE deleted_at IS NOT NULL AND EXISTS
(members)`, и метрика сходится к 0 по построению, пока UPDATE не упал.

Повторю границу, которую я и в исходном комментарии проводил: расширять скоуп на
MC-3803/MC-3806 не прошу. Прошу один замер, который отличает «UPDATE выполнился»
от «стало лучше» — этого в документе нет, поэтому тред остаётся открытым.
""")

# ------------------------------------------------------------- T7: not closed
reply(
    "b1823ce7957f53e4ca14125f2575d4eeedd39353", False,
    """
**Не закрываю: файл не тронут.**

`git diff c157f20f0..4a09875c8 -- internal/infrastructure/store/canonical_revalidation_repair.go`
— пусто. Полный список id для `skippedNonEmpty` уходит в лог как был.

Асимметрия с `upsert.go` при этом никуда не делась и стала заметнее: там
`orphanPreservedSampleSize` ограничивает выборку 20 записями и в комментарии
`upsert.go:34-39` объяснено почему («a log record holding every id would be
unreadable and expensive to ship»), а здесь та же по форме steady-state
популяция печатается целиком на каждом часовом проходе. Одно из двух решений
неверно; пока оба в дереве — тред открыт.
""")

# ------------------------------------------------------------- T8: not closed
reply(
    "23591926150227fd6c89d47f49ba49edf6404be4", False,
    """
**Не закрываю, но и не блокирую: ключ не менялся, тикета на него не завели.**

`preserved_canonical` на `upsert.go:1078` на HEAD тот же, и ветка, кладущая в
`preserved` группы с `!canonicalSet[id]`, на месте. Как я и писал, это
pre-existing drift, а не регрессия этого MR — поэтому 🟢 и «на потом», не блокер.

Что проверил дополнительно: MC-3804 к этому классу **не расширен**. В
`Task.md:177` он по-прежнему сформулирован узко — про `player_not_in_match_group`
и только. То есть «заведём в MC-3804» пока не произошло.

Держу открытым как трекер, а не как претензию к мержу: закрою в ту же секунду,
как ключ станет `preserved_total` (+ отдельный `preserved_canonical`) или как
MC-3804 явно охватит «log key lies about what it counts» целиком. На мерж этого
MR не влияет.
""")

# ------------------------------------------------------------- T9: not closed
reply(
    "a36904d7be5479bcbc15edf25a400f807968c2f6", False,
    """
**Не закрываю: тест появился, но `apply()` он по-прежнему не вызывает.**

В `4a09875c8` в `main_test.go` добавлен `TestApplyCounters_IncrementAfterSuccess`
(:542), и в нём честно написано, что он делает: «This **mirrors** `apply()`'s
accounting» (:552). Зеркало — не вызов. Он воспроизводит арифметику счётчиков в
теле теста; если она разъедется с `apply()`, тест этого не увидит, потому что
второй стороны сравнения у него нет.

Ни один из тестов в файле не открывает транзакцию и не идёт через
`pgxpool`/`pgx.Tx`, то есть `pg_advisory_xact_lock`, `FOR UPDATE`-перескан,
`store.ReviveGroupsByIDsTx` и сверка `RowsAffected` перед коммитом остаются
непокрытыми — а это единственный путь в CLI, который может потерять данные.

Признаю, что дистанция сократилась: раньше на счётчиках не было и зеркала.
Но исходное возражение было про интеграционный тест на `apply()`, и его нет.
""")

# --------------------------------------------------------------- T10: closed
reply(
    "939b71bfeb472c6fb56aa22654d4d1d6bc95e378", True,
    """
**Закрываю: порядок исправлен в `4a09875c8`.**

В `run()` на HEAD cap проверяется первым:

```
if previewSummary.Groups > opts.MaxGroups { ... }   // :545  — отказ
logPlans(ctx, logger, preview)                      // :553  — форензика
```

И на `:537-543` добавлен комментарий, фиксирующий, что это намеренный порядок,
а не побочный эффект перестановки. Форензик-шторм в сценарии «предикат зацепил
кратно больше ожидаемого» больше не случается: до `logPlans` управление не
доходит. Закрываю.
""")

# ------------------------------------------------------------ T11: not closed
reply(
    "5f839ea49fda8758dd261788c5ec8867bc03088d", False,
    """
**Не закрываю: детекции так и нет.**

Перепроверил на HEAD ровно тем же способом, что и в исходном комментарии:

- `git diff c157f20f0..4a09875c8 -- cmd/monitoring internal/metrics docker/grafana internal/interfaces/dashboard`
  — пусто;
- в `queries_quality_invariants.go` ничего не добавлено;
- LogQL-правила на Info-строку `matching: orphan groups preserved because they
  still hold members` нет.

Популяция вычищена, корневая причина закрыта `NOT EXISTS` — с этим спора нет.
Но приёмочный запрос остаётся ручным, и повторное появление stranded-групп
из другого пути (см. соседний тред про незащищённые `orphan_repair`-удаления)
будет обнаружено человеком, а не алертом. Это и было содержанием комментария,
и оно в силе.
""")


print(json.dumps({"iid": IID, "session_id": SESSION, "replies": R},
                 ensure_ascii=False, indent=2))
