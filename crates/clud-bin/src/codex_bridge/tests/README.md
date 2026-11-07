# Gateway HTTP redaction tests

`redaction_http.rs` covers raw free-model routing, outbound text/history/tool
fields, nonstreaming response restoration, split streaming tool arguments,
provider failover, native token counting and clean request byte preservation.
The parent test module owns the fake upstream and request helpers.
Run through Bosn CI as required by the repository root guidance.

`failed_turn_http.rs` covers failed-turn prompt recovery with existing or empty
canonical history, repeated failures, duplicate prevention, and real replies
mentioning “API Error”, using the same local fake Responses upstream.
