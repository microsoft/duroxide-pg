\* Copyright (c) Microsoft Corporation.
\* Licensed under the MIT License.

-------------------------- MODULE LeaseRedelivery --------------------------
EXTENDS LeaseProtocol

(*
Bounded continuation of the old-ack counterexample.
Slot 1 is A's original ack. Slot 2 is B's fetch and subsequent ack.
Slot 3 is a later fetch/ack by B, after slot 2 finished or rolled back.
No new message is enqueued. An initial QueueMessage/ExternalRaised arrival
must not be consumed a second time simply because its queue row survived.
Cancellation is disabled. B may fail its expiry lookup or history unique key;
the shortest unconstrained trace chooses the earlier expiry rejection.
*)

AckFetchFetch == <<"ack", "fetch", "fetch">>
RedeliveryOwners == <<1, 2, 2>>

NextRedelivery ==
    /\ Next
    /\ (tx[3].pc = "ready" /\ tx'[3].pc = "arrival" => tx[2].pc = "done")

SpecRedelivery == Init /\ [][NextRedelivery]_vars
=============================================================================
