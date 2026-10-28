/** The id of a run, as the pane's selection holds it. */
export type RunId = number

declare module 'claude-code' {
  interface PluginState {
    'companion-panes': {
      /** Bumped whenever a run's status or output changes, so the pane redraws. */
      rev: Shaped<number>
      /** What the pane's command field holds. */
      draft: Shaped<string>
      /** The id of the run whose output the pane shows. */
      selected: Shaped<RunId>
    }
  }
}

