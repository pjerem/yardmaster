Feature: Status reporting
  `yard status` renders a human table by default and a stable JSON report
  with --json, for scripting.

  Scenario: --json exposes a stable report
    Given a fresh state directory
    When I run "yard status --json"
    Then the command succeeds
    And the JSON daemon version matches the crate version
    And the JSON report lists no items

  Scenario: human output mentions the empty backlog
    Given a fresh state directory
    When I run "yard status"
    Then the command succeeds
    And the output mentions "no work items yet"
