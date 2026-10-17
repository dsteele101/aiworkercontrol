---
name: "why"
description: "Trace a rule in AGENTS.md or CLAUDE.md back to the fragment that defines it"
argument-hint: "<a phrase from the rule>"
allowed-tools: "Bash(agentcfg why:*)"
---

<!-- agentcfg:why · v1.0.1 -->
# Why this rule

Run `agentcfg why "$ARGUMENTS"` and report what it says.

The rules here are composed from fragments in a central release, so a
rule is not a file in this repository that can simply be edited. This
names the fragment that owns it, which is where a change has to be made,
and every repository selecting that fragment gets the change too.
