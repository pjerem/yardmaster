Feature: Ticket to pull request (E2E)
  `yard add` drives one ticket through worktree, agent, local checks, the
  automatic branch push (orange gate) and the create-pr red gate; the pull
  request only exists after an explicit human approval.

  Scenario: a ticket assigned to someone else is refused
    Given a fresh state directory
    And an e2e sandbox with the ticket assigned to "someone-else"
    When I run "yard add gh:acme/widgets#23"
    Then the command fails
    And the error output mentions "never touches someone else's ticket"
    And the daemon reports no work items

  Scenario: add stops at the create-pr gate with the branch pushed
    Given a fresh state directory
    And an e2e sandbox with the ticket assigned to "testuser"
    When I run "yard add gh:acme/widgets#23"
    Then the command succeeds
    And within 15 seconds a "create_pr" gate is pending
    And the work item worktree exists
    And the bare remote has branch "feature/acme-widgets-23-add-frobnicator"
    And the work item is in state "developing"
    And the forge received 0 PR creations

  Scenario: approving the gate creates the pull request
    Given a fresh state directory
    And an e2e sandbox with the ticket assigned to "testuser"
    When I run "yard add gh:acme/widgets#23"
    Then within 15 seconds a "create_pr" gate is pending
    When I run "yard approve 1"
    Then the command succeeds
    And the forge received exactly one PR creation for head "feature/acme-widgets-23-add-frobnicator" into "main" as a draft
    And the work item is in state "pr_pending"
    And the item event log records, in order: "state_changed:queued, state_changed:developing, branch_pushed, gate_requested, gate_approved, state_changed:pr_pending"

  Scenario: rejecting the gate escalates and never touches the forge
    Given a fresh state directory
    And an e2e sandbox with the ticket assigned to "testuser"
    When I run "yard add gh:acme/widgets#23"
    Then within 15 seconds a "create_pr" gate is pending
    When I run "yard reject 1 --reason not-today"
    Then the command succeeds
    And the forge received 0 PR creations
    And the work item is in state "escalated"
    And the event log records "gate_rejected"

  Scenario: yard logs surfaces the agent transcript
    Given a fresh state directory
    And an e2e sandbox with the ticket assigned to "testuser"
    When I run "yard add gh:acme/widgets#23"
    Then within 15 seconds a "create_pr" gate is pending
    When I run "yard logs 1"
    Then the command succeeds
    And the output mentions "stub agent starting"

  Scenario: a failing check command escalates before any push
    Given a fresh state directory
    And an e2e sandbox with the ticket assigned to "testuser"
    And the sandbox check command fails
    When I run "yard add gh:acme/widgets#23"
    Then the command succeeds
    And within 15 seconds the work item is in state "escalated"
    And the bare remote has no branch "feature/acme-widgets-23-add-frobnicator"
    And the event log records "check_failed"
