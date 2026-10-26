\* Copyright (c) Microsoft Corporation.
\* Licensed under the MIT License.

------------------------- MODULE LeaseReleaseRace --------------------------
EXTENDS LeaseProtocol

(*
Two concurrent calls on the same fetched token: foreground acknowledgement
and abandonment. This specifically exercises FIXED's instance-delete count.
Abandon has no advisory lock and can release the token while ack holds ADV.
The queue DELETE can then match zero rows; the final instance DELETE must
reject the acknowledgement and roll its history/output writes back.
*)

AckAbandon == <<"ack", "abandon">>
=============================================================================
