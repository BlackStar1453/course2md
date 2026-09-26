# course2md

Turns a course video into reading notes: transcript text split into sections, each with a screenshot and a timestamp that links back to the video.

## Language

### Notes

**Section**:
One stretch of the video between two picture changes, holding one screenshot and the transcript spoken during it.
_Avoid_: slide, chapter, segment

**Segment**:
One timed line of transcript text inside a Section; the unit that proofreading reads and rewrites.
_Avoid_: caption, line, sentence

### AI services

**Provider**:
The way course2md reaches a language model: which service it talks to and how it authenticates.
One vendor can back more than one Provider (e.g. Codex over its web API vs. Codex through its CLI).
_Avoid_: vendor, supplier, backend

**CLI Provider**:
A Provider that runs a locally installed, logged-in assistant program (Claude Code, Codex) and uses its subscription instead of an API key.
_Avoid_: agent provider, local provider

**Codex Web Provider**:
The original Codex Provider: course2md keeps its own copy of the Codex sign-in and calls the Codex web API directly.
_Avoid_: Codex login, Codex (unqualified)

**Codex CLI Provider**:
The CLI Provider that runs the installed `codex` program; the program keeps its own sign-in.
_Avoid_: Codex (unqualified)

**Claude Code Provider**:
The CLI Provider that runs the installed `claude` program with the user's Claude subscription.
_Avoid_: Claude, Anthropic provider

**Proofreading**:
Optional AI pass that corrects the Segments' text without changing their timing; empty output removes a Segment.
_Avoid_: polishing, editing, cleanup

**Summary**:
Optional AI pass that produces a TL;DR, key points and a timed outline for the whole note.
_Avoid_: abstract, recap

**Vision**:
Sending a Section's screenshot alongside its Segments so Proofreading can use what is on screen.
_Avoid_: image mode, multimodal

## Relationships

- A **Section** has one screenshot and one or more **Segments**.
- **Proofreading** works on batches of **Segments**; with **Vision**, a batch stays inside one **Section** and carries its screenshot.
- Every AI pass uses exactly one **Provider**; a **CLI Provider** is one kind of **Provider**.

## Flagged ambiguities

- "供应方 / supplier" was used for **Provider**; resolved: a Provider is a way of reaching a model, not a company.
- "Codex" alone is ambiguous between **Codex Web Provider** and **Codex CLI Provider**; always say which.
