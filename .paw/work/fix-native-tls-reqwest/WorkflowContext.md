# WorkflowContext

Work Title: Fix Native TLS Reqwest
Work ID: fix-native-tls-reqwest
Base Branch: main
Target Branch: fix/native-tls-reqwest-feature
Execution Mode: current-checkout
Repository Identity: github.com/microsoft/duroxide-pg@dbf922cdd64883871920019650e338a4f5af5ae1
Execution Binding: none
Workflow Mode: minimal
Review Strategy: local
Review Policy: final-pr-only
Session Policy: continuous
Final Agent Review: disabled
Final Review Mode: multi-model
Final Review Interactive: smart
Final Review Models: latest GPT, latest Gemini, latest Claude Opus
Final Review Specialists: all
Final Review Interaction Mode: parallel
Final Review Specialist Models: none
Final Review Perspectives: auto
Final Review Perspective Cap: 2
Implementation Model: none
Plan Generation Mode: single-model
Plan Generation Models: latest GPT, latest Gemini, latest Claude Opus
Planning Docs Review: disabled
Planning Review Mode: multi-model
Planning Review Interactive: smart
Planning Review Models: latest GPT, latest Gemini, latest Claude Opus
Planning Review Specialists: all
Planning Review Interaction Mode: parallel
Planning Review Specialist Models: none
Planning Review Perspectives: auto
Planning Review Perspective Cap: 2
Custom Workflow Instructions: none
Initial Prompt: Add native-tls feature to reqwest dep so the Rust HTTP client has a TLS connector. Without this, every HTTPS call (incl. WorkloadIdentityCredential and AAD token endpoint) fails with hyper's 'invalid URL, scheme is not http' — making the Entra credential chain unusable on AKS pods. Reproduced E2E in PilotSwarm AKS deploy; verified fix preserves native-tls/openssl posture (no rustls/ring/aws-lc-rs added).
Issue URL: none
Remote: origin
Artifact Lifecycle: commit-and-clean
Artifact Paths: auto-derived
Additional Inputs: none
