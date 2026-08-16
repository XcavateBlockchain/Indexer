//! Serves the GraphiQL IDE page for `GET /graphiql`.
//!
//! Deliberately does NOT use `juniper_axum::graphiql` -- the template embedded in juniper
//! 0.16.2 pairs `graphiql@3.1.2` with `@graphiql/plugin-explorer@4.0.6`, which are
//! incompatible: the explorer plugin calls `useSchemaStore`, an API that only exists in
//! `@graphiql/react` >= 0.34, while graphiql 3.1.2 bundles 0.20. The page white-screens with
//! `TypeError: n.useSchemaStore is not a function` before anything renders. Fixed on juniper
//! master (unreleased as of 0.16.2) by moving to an esm.sh import map that pins one coherent
//! version set; [`PAGE`] is that template with the `/graphql` endpoint baked in and the unused
//! subscription wiring dropped. If juniper ships a release containing the fix, this module can
//! be deleted and the route pointed back at `juniper_axum::graphiql`.

use axum::response::Html;

pub async fn graphiql() -> Html<&'static str> {
    Html(PAGE)
}

/// Adapted from juniper master's `juniper/src/http/graphiql.html` (MIT, GraphQL Contributors).
const PAGE: &str = r#"<!--
 *  Copyright (c) 2025 GraphQL Contributors
 *  All rights reserved.
 *
 *  This source code is licensed under the license found in the
 *  LICENSE file in the root directory of this source tree.
-->
<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>GraphiQL</title>
    <style>
      body {
        margin: 0;
      }

      #graphiql {
        height: 100dvh;
      }

      .loading {
        height: 100%;
        display: flex;
        align-items: center;
        justify-content: center;
        font-size: 4rem;
      }
    </style>
    <link
      rel="stylesheet"
      href="https://esm.sh/graphiql@5.2.4/dist/style.css"
      crossorigin="anonymous"
    />
    <link
      rel="stylesheet"
      href="https://esm.sh/@graphiql/plugin-explorer@5.1.3/dist/style.css"
      crossorigin="anonymous"
    />
    <!--
     * Note:
     * The ?standalone flag bundles the module along with all of its `dependencies`, excluding `peerDependencies`, into a single JavaScript file.
     * `@emotion/is-prop-valid` is a shim to remove the console error ` module "@emotion /is-prop-valid" not found`. Upstream issue: https://github.com/motiondivision/motion/issues/3126
    -->
    <script type="importmap">
      {
        "imports": {
          "react": "https://esm.sh/react@19.2.5",
          "react/": "https://esm.sh/react@19.2.5/",
          "react-dom": "https://esm.sh/react-dom@19.2.5",
          "react-dom/": "https://esm.sh/react-dom@19.2.5/",
          "graphiql": "https://esm.sh/graphiql@5.2.4?standalone&external=react,react-dom,@graphiql/react,graphql",
          "graphiql/": "https://esm.sh/graphiql@5.2.4/",
          "@graphiql/plugin-explorer": "https://esm.sh/@graphiql/plugin-explorer@5.1.3?standalone&external=react,@graphiql/react,graphql",
          "@graphiql/react": "https://esm.sh/@graphiql/react@0.37.7?standalone&external=react,react-dom,graphql,@graphiql/toolkit,@emotion/is-prop-valid",
          "@graphiql/toolkit": "https://esm.sh/@graphiql/toolkit@0.12.1?standalone&external=graphql",
          "graphql": "https://esm.sh/graphql@16.13.2",
          "@emotion/is-prop-valid": "data:text/javascript,"
        }
      }
    </script>
    <script type="module">
      import React from 'react';
      import ReactDOM from 'react-dom/client';
      import { GraphiQL, HISTORY_PLUGIN } from 'graphiql';
      import { createGraphiQLFetcher } from '@graphiql/toolkit';
      import { explorerPlugin } from '@graphiql/plugin-explorer';
      import 'graphiql/setup-workers/esm.sh';

      const fetcher = createGraphiQLFetcher({ url: '/graphql' });
      const plugins = [HISTORY_PLUGIN, explorerPlugin()];

      function App() {
        return React.createElement(GraphiQL, {
          fetcher,
          plugins,
          defaultEditorToolsVisibility: true,
        });
      }

      const container = document.getElementById('graphiql');
      const root = ReactDOM.createRoot(container);
      root.render(React.createElement(App));
    </script>
  </head>
  <body>
    <div id="graphiql">
      <div class="loading">Loading…</div>
    </div>
  </body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::PAGE;

    /// The IDE must post to the same path `crate::router` mounts the executor on.
    #[test]
    fn page_targets_the_graphql_endpoint() {
        assert!(PAGE.contains("createGraphiQLFetcher({ url: '/graphql' })"));
    }

    /// Guards against reintroducing the juniper 0.16.2 pairing (graphiql 3.x UMD next to
    /// plugin-explorer 4.x) that white-screens on `useSchemaStore`.
    #[test]
    fn page_pins_a_coherent_graphiql_version_set() {
        assert!(PAGE.contains("esm.sh/graphiql@5.2.4"));
        assert!(PAGE.contains("esm.sh/@graphiql/plugin-explorer@5.1.3"));
        assert!(PAGE.contains("esm.sh/@graphiql/react@0.37.7"));
        assert!(!PAGE.contains("graphiql@3."));
        assert!(!PAGE.contains("plugin-explorer@4."));
    }
}
