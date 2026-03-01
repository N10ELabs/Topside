## Section: Phase 2 - Broader Context Import and Background Sync
- [ ] Add support for selected `docs/` content for notes section
## Section: Phase 3 - Optional Bidirectional Sync
- [ ] Expand import beyond `to-do.md`
- [ ] Define conflict policy between repo files and `n10e`
- [ ] Define ownership rules for synced tasks
- [ ] Add safe write-back support for `to-do.md`
- [ ] Support manual reconcile flows for conflicting edits
- [ ] Add guardrails before enabling bidirectional sync by default
## Section: Phase 4 - Agentic Task Assignments
- [ ] Define agent capability metadata and assignment eligibility rules
- [ ] Add explicit task assignment controls for agent ownership
- [ ] Enable claim, assign, and release flows in MCP task operations
- [ ] Surface assignment state and unassigned queues in the UI
- [ ] Record assignment audit events and guardrails for automated reassignment
## Section: Phase 5 - Autonomous Multi-Agent Orchestration
- [ ] Add policy-driven automatic routing for unassigned tasks
- [ ] Track agent capacity and concurrency limits before auto-assignment
- [ ] Add escalation and fallback queues when assigned work stalls
- [ ] Measure assignment outcomes to improve routing heuristics
## Section: Phase 6 - Visual Polish and Functionality
- [ ] Continue to improve latency in tool calling for MCP
- [ ] Add buttons for to-do pane
- [x] Improve project pane visuals
- [ ] Connect any unfinished options in project settings menus
- [ ] Correct pane title placement and find alternative for dropdown chevron
- [ ] Ensure scroll position does not change on task highlight
- [ ] Put headers with borders and indent the tasks as clean no border rows
- [ ] Remove the lines below pane titles. Cleaner UI
- [ ] Visual animation when tasks are created or destroyed by agent in todo.md
## Phase 7 - Hub as Interactive Codebase
- [ ] Create bidirectional sync as to-do.md
- [ ] Update todo pane as to-do.md
- [ ] Update notes as /notes (this will later sync to docs
- [ ] Consider deprecating current MCP read/write since its no longer needed
- [ ] Initial project selection should initilaize todo.md in project workspace
