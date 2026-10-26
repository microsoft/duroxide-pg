\* Copyright (c) Microsoft Corporation.
\* Licensed under the MIT License.

--------------------------- MODULE LeaseProtocol ---------------------------
EXTENDS Integers, Sequences, FiniteSets, TLC

(*
One-instance, statement-interleaving model of duroxide-pg orchestration leases.
MAIN = 3beba92048b9fd6c9e68262a3e3fa192441c1e31
PR43 = b2ff29633a5ea437f8f1e737429d7ba9170ba491
FIXED = ea598d91839b6d656917a47a3b4e772fa515b9aa
Its ack takes ADV, rechecks the token, and checks the INSTANCE delete count.
The queue delete count is not checked in this committed source.

This is an executable abstraction, not an implementation refinement proof.
SQL actions read committed state at each statement; writes remain private
until Commit. Row/advisory locks last until Commit or Rollback. A blocked
UPDATE/DELETE rechecks its token predicate when it can proceed (EvalPlanQual).

The initial state is the state just after worker 1 fetched one visible batch.
Each call slot executes its selected procedure once. A successful fetch may
then acknowledge its captured inputs/history. Two slots may belong to the
same worker: its foreground turn and its background renewal are concurrent.
Every integer worker skew in [-MaxSkew, MaxSkew] is chosen at initialization.
Skews are constant, database time is monotonic, and a tick may occur between
any statements, including between validation, writes, and commit.

Q represents a nonempty batch whose rows have the same token and deadline.
Batch DML acquires its rows in one common order and is collapsed to one lock.
H includes the instance/execution metadata row lock preceding history INSERT,
and enforces history's (instance, execution, event_id) unique constraint.
Each modeled ack appends one history position and emits one durable output.
Messages model QueueMessage/ExternalRaised arrivals, not deduplicated
completion events. No activity executor or external side effect is modeled.
*)

CONSTANTS Variant, N, Messages, L, R, MaxSkew, Horizon,
          Kinds, Owners, FetchThenAck, AllowCancellation

Workers == 1..N
Procs == DOMAIN Kinds
Resources == {"ADV", "Q", "I", "H"}
NoProc == 0

AckFetch == <<"ack", "fetch">>
RenewFetch == <<"renew", "fetch">>
RenewAck == <<"renew", "ack">>
RenewAbandon == <<"renew", "abandon">>
AckFetchRenew == <<"ack", "fetch", "renew">>
AbandonFetchRenew == <<"abandon", "fetch", "renew">>
RenewOnly == <<"renew">>
TwoWorkers == <<1, 2>>
OneWorkerTwoCalls == <<1, 1>>
TwoWorkersThreeCalls == <<1, 2, 1>>
OneWorker == <<1>>

Max(a, b) == IF a >= b THEN a ELSE b
Min(a, b) == IF a <= b THEN a ELSE b

ASSUME /\ Variant \in {"MAIN", "PR43", "FIXED"}
       /\ N \in Nat \ {0}
       /\ Messages # {}
       /\ L > 0 /\ R > 0 /\ R < L
       /\ MaxSkew \in Nat /\ Horizon \in Nat
       /\ DOMAIN Owners = Procs
       /\ \A p \in Procs : Owners[p] \in Workers
       /\ \A p \in Procs : Kinds[p] \in {"ack", "fetch", "renew", "abandon"}
       /\ FetchThenAck \in BOOLEAN /\ AllowCancellation \in BOOLEAN

VARIABLES tick, skew, lease, queue, history, outputs, tx, locks, audit
vars == <<tick, skew, lease, queue, history, outputs, tx, locks, audit>>

DBNow == MaxSkew + tick
WorkerNow(w) == DBNow + skew[w]
SeedDeadline == MaxSkew + Max(skew[1], 0) + L
FreshNow(p) == Max(tx[p].raw + DBNow - tx[p].entered, DBNow)

EmptyTx(op, pc, tok, base, inputs) ==
    [op |-> op, pc |-> pc, tok |-> tok, base |-> base, inputs |-> inputs,
     raw |-> 0, entered |-> 0, deadline |-> 0,
     iw |-> FALSE, it |-> 0, iu |-> 0,
     qw |-> FALSE, qe |-> TRUE, qt |-> 0, qu |-> 0,
     hw |-> FALSE, ow |-> FALSE,
     oldUntil |-> 0, checkClock |-> 0, checkedAt |-> 0, wroteAt |-> 0]

Init ==
    /\ tick = 0
    /\ skew \in [Workers -> ((-MaxSkew)..MaxSkew)]
    /\ lease = [token |-> 1, until |-> SeedDeadline]
    /\ queue = [exists |-> TRUE, token |-> 1, until |-> SeedDeadline]
    /\ history = <<>>
    /\ outputs = <<>>
    /\ tx = [p \in Procs |->
          EmptyTx(Kinds[p], "ready",
                  IF Kinds[p] = "fetch" THEN p + 1 ELSE 1,
                  0, IF Kinds[p] = "fetch" THEN {} ELSE Messages)]
    /\ locks = [r \in Resources |-> NoProc]
    /\ audit = [renewFenced |-> TRUE, ackFenced |-> TRUE,
                neverShortened |-> TRUE, checkedLive |-> TRUE,
                liveAtWrite |-> TRUE, oldLiveAtCommit |-> TRUE,
                renewalDeadlineLive |-> TRUE, fetchDeadlineLive |-> TRUE]

CanLock(p, r) == locks[r] \in {NoProc, p}
OwnLease(p) == lease.token = tx[p].tok
OwnQueue(p) == queue.exists /\ queue.token = tx[p].tok
Fetchable(p) == queue.exists /\ (lease.token = 0 \/ lease.until <= tx[p].raw)
Release(p) == [r \in Resources |-> IF locks[r] = p THEN NoProc ELSE locks[r]]
PC(p, label) == [tx EXCEPT ![p].pc = label]
Raise(p) == PC(p, "rollback")

Tick ==
    /\ tick < Horizon
    /\ tick' = tick + 1
    /\ UNCHANGED <<skew, lease, queue, history, outputs, tx, locks, audit>>

\* Rust now_millis() precedes pool acquisition; entered is statement_timestamp.
Begin(p) ==
    /\ tx[p].pc = "ready"
    /\ (tx[p].op # "renew" \/ tick >= R)
    /\ tx' = [tx EXCEPT ![p].pc = "arrival", ![p].raw = WorkerNow(Owners[p])]
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

Arrival(p) ==
    /\ tx[p].pc = "arrival"
    /\ tx' = [tx EXCEPT ![p].entered = DBNow,
          ![p].pc = CASE tx[p].op = "fetch" -> "f.candidate"
                      [] tx[p].op = "ack" -> "a.lookup"
                      [] tx[p].op = "abandon" -> "b.lookup"
                      [] Variant = "MAIN" -> "r.mainI"
                      [] OTHER -> "r.lookup"]
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* 0025:88-96 -- plain candidate SELECT.
FetchCandidate(p) ==
    /\ tx[p].pc = "f.candidate"
    /\ tx' = IF Fetchable(p) THEN PC(p, "f.advisory") ELSE Raise(p)
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* 0025:104; 0026:62; fixed ack's advisory lock.
Advisory(p) ==
    /\ tx[p].pc \in {"f.advisory", "r.advisory", "a.advisory"}
    /\ CanLock(p, "ADV")
    /\ locks' = [locks EXCEPT !["ADV"] = p]
    /\ tx' = PC(p, CASE tx[p].op = "fetch" -> "f.queueSelect"
                      [] tx[p].op = "renew" -> "r.now"
                      [] OTHER -> "a.recheck")
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* 0025:127-139 -- FOR UPDATE SKIP LOCKED; it does not wait on Q.
\* A failed one-instance candidate terminates this bounded attempt.
FetchQueueSelect(p) ==
    /\ tx[p].pc = "f.queueSelect"
    /\ IF Fetchable(p) /\ CanLock(p, "Q")
          THEN /\ locks' = [locks EXCEPT !["Q"] = p]
               /\ tx' = PC(p, "f.computeLease")
          ELSE /\ locks' = locks
               /\ tx' = Raise(p)
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* 0025:146-151 -- separate from INSERT: time can pass before the write.
FetchComputeLease(p) ==
    /\ tx[p].pc = "f.computeLease"
    /\ tx' = [tx EXCEPT ![p].deadline = FreshNow(p) + L, ![p].pc = "f.instance"]
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* 0025:153-165 -- ON CONFLICT rechecks expiry after a row-lock wait.
FetchInstance(p) ==
    /\ tx[p].pc = "f.instance"
    /\ CanLock(p, "I")
    /\ locks' = [locks EXCEPT !["I"] = p]
    /\ tx' = IF lease.token = 0 \/ lease.until <= tx[p].raw
          THEN [tx EXCEPT ![p].iw = TRUE, ![p].it = tx[p].tok,
                  ![p].iu = tx[p].deadline, ![p].pc = "f.queueUpdate"]
          ELSE Raise(p)
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* 0025:168-174 -- write only unowned/expired visible rows.
FetchQueueUpdate(p) ==
    /\ tx[p].pc = "f.queueUpdate"
    /\ CanLock(p, "Q")
    /\ locks' = [locks EXCEPT !["Q"] = p]
    /\ tx' = IF queue.exists /\ (queue.token = 0 \/ queue.until <= tx[p].raw)
          THEN [tx EXCEPT ![p].qw = TRUE, ![p].qe = TRUE,
                  ![p].qt = tx[p].tok, ![p].qu = tx[p].deadline,
                  ![p].pc = "f.messages"]
          ELSE PC(p, "f.messages")
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* 0025:177-181 -- own writes are visible inside the transaction.
FetchMessages(p) ==
    /\ tx[p].pc = "f.messages"
    /\ tx' = [tx EXCEPT ![p].inputs =
          IF tx[p].qw /\ tx[p].qe /\ tx[p].qt = tx[p].tok THEN Messages ELSE {},
          ![p].pc = "f.history"]
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* 0025:191-201 -- a new READ COMMITTED snapshot, no H row lock.
FetchHistory(p) ==
    /\ tx[p].pc = "f.history"
    /\ tx' = [tx EXCEPT ![p].base = Len(history), ![p].pc = "commit"]
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* 0020:74-80; 0026:53-60; 0006:101-107.
Lookup(p) ==
    /\ tx[p].pc \in {"a.lookup", "r.lookup", "b.lookup"}
    /\ tx' = IF OwnLease(p) /\ (tx[p].op = "abandon" \/ lease.until > tx[p].raw)
          THEN PC(p, CASE tx[p].op = "renew" -> "r.advisory"
                       [] tx[p].op = "abandon" -> "b.queue"
                       [] Variant = "FIXED" -> "a.advisory"
                       [] OTHER -> "a.history")
          ELSE Raise(p)
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* Fixed ack checks the token again after acquiring ADV, not the new time.
AckRecheck(p) ==
    /\ tx[p].pc = "a.recheck"
    /\ tx' = IF OwnLease(p) THEN PC(p, "a.history") ELSE Raise(p)
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* 0020:93-126 -- metadata serialization and unique history position INSERT.
AckHistory(p) ==
    /\ tx[p].pc = "a.history"
    /\ CanLock(p, "H")
    /\ locks' = [locks EXCEPT !["H"] = p]
    /\ tx' = IF tx[p].base = Len(history)
          THEN [tx EXCEPT ![p].hw = TRUE, ![p].pc = "a.outputs"]
          ELSE Raise(p)
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* 0020:221-291 -- durable output staging, not external effect execution.
AckOutputs(p) ==
    /\ tx[p].pc = "a.outputs"
    /\ tx' = [tx EXCEPT ![p].ow = TRUE, ![p].pc = "a.queue"]
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* 0020:295; 0006:141-149. A token mismatch means zero rows, not a lock.
ReleaseQueue(p) ==
    /\ tx[p].pc \in {"a.queue", "b.queue"}
    /\ ~OwnQueue(p) \/ CanLock(p, "Q")
    /\ locks' = IF OwnQueue(p) THEN [locks EXCEPT !["Q"] = p] ELSE locks
    /\ tx' = IF OwnQueue(p)
          THEN [tx EXCEPT ![p].qw = TRUE,
                  ![p].qe = (tx[p].op = "abandon"), ![p].qt = 0, ![p].qu = 0,
                  ![p].pc = IF tx[p].op = "ack" THEN "a.instance" ELSE "b.instance"]
          ELSE PC(p, IF tx[p].op = "ack" THEN "a.instance" ELSE "b.instance")
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* 0020:298-299; 0006:148-149. FIXED raises on a zero-row ack DELETE.
ReleaseInstance(p) ==
    /\ tx[p].pc \in {"a.instance", "b.instance"}
    /\ ~OwnLease(p) \/ CanLock(p, "I")
    /\ locks' = IF OwnLease(p) THEN [locks EXCEPT !["I"] = p] ELSE locks
    /\ tx' = IF OwnLease(p)
          THEN [tx EXCEPT ![p].iw = TRUE, ![p].it = 0, ![p].iu = 0, ![p].pc = "commit"]
          ELSE IF Variant = "FIXED" /\ tx[p].op = "ack"
               THEN Raise(p)
               ELSE PC(p, "commit")
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* MAIN 0004:349-359 -- raw caller time; I before Q; may shorten.
MainRenewInstance(p) ==
    /\ tx[p].pc = "r.mainI"
    /\ ~OwnLease(p) \/ CanLock(p, "I")
    /\ locks' = IF OwnLease(p) THEN [locks EXCEPT !["I"] = p] ELSE locks
    /\ tx' = IF OwnLease(p) /\ lease.until > tx[p].raw
          THEN [tx EXCEPT ![p].iw = TRUE, ![p].it = tx[p].tok,
                  ![p].iu = tx[p].raw + L, ![p].deadline = tx[p].raw + L,
                  ![p].oldUntil = lease.until, ![p].checkClock = tx[p].raw,
                  ![p].checkedAt = DBNow,
                  ![p].wroteAt = DBNow, ![p].pc = "r.queue"]
          ELSE Raise(p)
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* PR43 0026:64-67 / FIXED 0026:76-79 -- PL/pgSQL time assignment.
\* This is distinct from the following SELECT: a stall can occur between them.
RenewSampleNow(p) ==
    /\ tx[p].pc = "r.now"
    /\ tx' = [tx EXCEPT ![p].checkClock = FreshNow(p), ![p].pc = "r.recheck"]
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* PR43 0026:69-78 -- token/expiry check using the previously sampled time.
RenewRecheck(p) ==
    /\ tx[p].pc = "r.recheck"
    /\ tx' = IF OwnLease(p) /\ lease.until > tx[p].checkClock
          THEN [tx EXCEPT ![p].deadline = Max(lease.until, tx[p].checkClock + L),
                  ![p].oldUntil = lease.until, ![p].checkedAt = DBNow,
                  ![p].pc = "r.queue"]
          ELSE Raise(p)
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, locks, audit>>

\* MAIN 0004:362-364; PR43 0026:80-82.
RenewQueue(p) ==
    /\ tx[p].pc = "r.queue"
    /\ ~OwnQueue(p) \/ CanLock(p, "Q")
    /\ locks' = IF OwnQueue(p) THEN [locks EXCEPT !["Q"] = p] ELSE locks
    /\ tx' = IF OwnQueue(p)
          THEN [tx EXCEPT ![p].qw = TRUE, ![p].qe = TRUE,
                  ![p].qt = tx[p].tok, ![p].qu = tx[p].deadline,
                  ![p].pc = IF Variant = "MAIN" THEN "commit" ELSE "r.instance"]
          ELSE PC(p, IF Variant = "MAIN" THEN "commit" ELSE "r.instance")
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* PR43 0026:84-93 -- token recheck, zero rows raises, no fresh expiry test.
RenewInstance(p) ==
    /\ tx[p].pc = "r.instance"
    /\ ~OwnLease(p) \/ CanLock(p, "I")
    /\ locks' = IF OwnLease(p) THEN [locks EXCEPT !["I"] = p] ELSE locks
    /\ tx' = IF OwnLease(p)
          THEN [tx EXCEPT ![p].iw = TRUE, ![p].it = tx[p].tok,
                  ![p].iu = tx[p].deadline, ![p].wroteAt = DBNow, ![p].pc = "commit"]
          ELSE Raise(p)
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

\* All private writes become visible together; none appear on Rollback.
Commit(p) ==
    /\ tx[p].pc = "commit"
    /\ lease' = IF tx[p].iw THEN [token |-> tx[p].it, until |-> tx[p].iu] ELSE lease
    /\ queue' = IF tx[p].qw
          THEN [exists |-> tx[p].qe, token |-> tx[p].qt, until |-> tx[p].qu] ELSE queue
    /\ history' = IF tx[p].hw
          THEN Append(history, [base |-> tx[p].base, token |-> tx[p].tok,
                               inputs |-> tx[p].inputs]) ELSE history
    /\ outputs' = IF tx[p].ow
          THEN Append(outputs, [base |-> tx[p].base, token |-> tx[p].tok,
                               inputs |-> tx[p].inputs]) ELSE outputs
    /\ audit' = [audit EXCEPT
          !.renewFenced = @ /\ (tx[p].op # "renew" \/ OwnLease(p)),
          !.ackFenced = @ /\ (tx[p].op # "ack" \/ OwnLease(p)),
          !.neverShortened = @ /\ (tx[p].op # "renew" \/ tx[p].iu >= lease.until),
          !.checkedLive = @ /\ (tx[p].op # "renew" \/ tx[p].oldUntil > tx[p].checkedAt),
          !.liveAtWrite = @ /\ (tx[p].op # "renew" \/ tx[p].oldUntil > tx[p].wroteAt),
          !.oldLiveAtCommit = @ /\ (tx[p].op # "renew" \/ tx[p].oldUntil > DBNow),
          !.renewalDeadlineLive = @ /\ (tx[p].op # "renew" \/ tx[p].iu > DBNow),
          !.fetchDeadlineLive = @ /\ (tx[p].op # "fetch" \/ tx[p].iu > DBNow)]
    /\ locks' = Release(p)
    /\ tx' = [tx EXCEPT ![p] =
          IF tx[p].op = "fetch" /\ FetchThenAck /\ tx[p].inputs # {}
          THEN EmptyTx("ack", "ready", tx[p].tok, tx[p].base, tx[p].inputs)
          ELSE EmptyTx("none", "done", 0, 0, {})]
    /\ UNCHANGED <<tick, skew>>

Need(p) ==
    CASE tx[p].pc \in {"f.advisory", "r.advisory", "a.advisory"} -> "ADV"
      [] tx[p].pc = "f.instance" -> "I"
      [] tx[p].pc \in {"a.instance", "b.instance", "r.instance", "r.mainI"}
           /\ OwnLease(p) -> "I"
      [] tx[p].pc \in {"a.queue", "b.queue", "r.queue"} /\ OwnQueue(p) -> "Q"
      [] tx[p].pc = "f.queueUpdate" -> "Q"
      [] tx[p].pc = "a.history" -> "H"
      [] OTHER -> "NONE"

WaitsFor(p) ==
    IF Need(p) = "NONE" THEN NoProc
    ELSE IF CanLock(p, Need(p)) THEN NoProc ELSE locks[Need(p)]

RECURSIVE Follow(_, _)
Follow(p, k) ==
    IF k = 0 \/ p = NoProc THEN p ELSE Follow(WaitsFor(p), k - 1)
Cyclic(p) == \E k \in 1..Cardinality(Procs) : Follow(p, k) = p
NoDeadlock == ~(\E p \in Procs : Cyclic(p))

\* PostgreSQL deadlock detection may choose any cycle member as victim.
\* Cancellation before commit is one possible outcome, not a rollback promise.
Rollback(p) ==
    /\ tx[p].pc = "rollback" \/ Cyclic(p)
       \/ (AllowCancellation /\ tx[p].pc \notin {"ready", "done"})
    /\ locks' = Release(p)
    /\ tx' = [tx EXCEPT ![p] = EmptyTx("none", "done", 0, 0, {})]
    /\ UNCHANGED <<tick, skew, lease, queue, history, outputs, audit>>

Next ==
    \/ Tick
    \/ \E p \in Procs :
          \/ Begin(p) \/ Arrival(p) \/ FetchCandidate(p) \/ Advisory(p)
          \/ FetchQueueSelect(p) \/ FetchComputeLease(p) \/ FetchInstance(p)
          \/ FetchQueueUpdate(p) \/ FetchMessages(p) \/ FetchHistory(p)
          \/ Lookup(p) \/ AckRecheck(p) \/ AckHistory(p) \/ AckOutputs(p)
          \/ ReleaseQueue(p) \/ ReleaseInstance(p)
          \/ MainRenewInstance(p) \/ RenewSampleNow(p) \/ RenewRecheck(p) \/ RenewQueue(p)
          \/ RenewInstance(p) \/ Commit(p) \/ Rollback(p)

Spec == Init /\ [][Next]_vars

Range(s) == {s[i] : i \in DOMAIN s}

TypeOK ==
    /\ tick \in 0..Horizon
    /\ skew \in [Workers -> ((-MaxSkew)..MaxSkew)]
    /\ lease.token \in 0..(Cardinality(Procs) + 1) /\ lease.until \in Int
    /\ queue.exists \in BOOLEAN /\ queue.token \in 0..(Cardinality(Procs) + 1)
    /\ queue.until \in Int
    /\ DOMAIN tx = Procs
    /\ locks \in [Resources -> (Procs \cup {NoProc})]
    /\ \A h \in Range(history) : h.inputs \subseteq Messages

\* "Still deliverable" includes any retained input, even temporarily leased.
ExactlyOnceConsumption ==
    \A h \in Range(history) : h.inputs # {} => ~queue.exists
SingleWriter ==
    \A i, j \in DOMAIN history : i # j => history[i].base # history[j].base
NoDuplicateConsumption ==
    \A m \in Messages :
        Cardinality({i \in DOMAIN history : m \in history[i].inputs}) <= 1
AtomicOutputs == history = outputs
NoMessageLoss ==
    queue.exists \/ UNION {h.inputs : h \in Range(history)} = Messages
QueueLeaseAgreement ==
    queue.exists /\ queue.token # 0 =>
        queue.token = lease.token /\ queue.until = lease.until
NoStaleRenewal == audit.renewFenced
NoStaleAck == audit.ackFenced
NeverShorten == audit.neverShortened
NoExpiredRenewalAtCheck == audit.checkedLive
LeaseSafety == NoStaleRenewal /\ NeverShorten

\* Stronger wall-clock properties: deliberately NOT assumed in CoreSafety.
NoExpiredRenewalAtWrite == audit.liveAtWrite
NoExpiredRenewalAtCommit == audit.oldLiveAtCommit
RenewalDeadlineLiveAtCommit == audit.renewalDeadlineLive
FetchDeadlineLiveAtCommit == audit.fetchDeadlineLive
StrictLeaseSafety == LeaseSafety /\ NoExpiredRenewalAtCheck /\ NoExpiredRenewalAtWrite

StructuralSafety ==
    TypeOK /\ SingleWriter /\ AtomicOutputs /\ NoMessageLoss /\ QueueLeaseAgreement
FencedSafety ==
    StructuralSafety /\ LeaseSafety /\ NoDeadlock
CoreSafety ==
    FencedSafety /\ ExactlyOnceConsumption /\ NoDuplicateConsumption /\ NoStaleAck
=============================================================================
