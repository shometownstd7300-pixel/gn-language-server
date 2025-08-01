# GN Language Server Architecture

This document outlines the architecture and key design decisions of the GN Language Server.

## 1. Overview

The primary goal of this language server is to provide a fast and useful IDE experience for the GN build system. It is written in **Rust** and built on top of several key libraries:
-   **`tower-lsp`**: For the core Language Server Protocol framework.
-   **`pest`**: For parsing the GN language based on a formal grammar.
-   **`tokio`**: For asynchronous I/O and concurrency.

## 2. Key Design Decisions

Several core design decisions shape the server's behavior and performance.

### Configuration-Agnostic Analysis (Ignoring `args.gn`)

The most fundamental design choice is that **the server does not read `args.gn` files**. It analyzes the build files in a configuration-agnostic way, without knowing the final values of build arguments for any specific output directory (e.g., `out/Debug`).

This is a deliberate trade-off that prioritizes simplicity and a holistic editing experience over configuration-specific precision.

**Pros:**
-   **Simplicity & Decoupling**: The server does not need to track which of the potentially many build directories is "active," simplifying state management and user configuration. It works out of the box.
-   **Holistic Code View**: By not evaluating conditionals, the server analyzes all possible code paths. This is ideal for developers who need to understand and refactor code that spans multiple configurations (e.g., `if (is_win)` and `if (is_linux)`).
-   **Performance & Stability**: The analysis is stable and depends only on the contents of the `.gn` and `.gni` source files. It avoids the high cost of a full build config evaluation. This means the analysis can be done quickly without costly evaluations like `exec_script()`.

**Cons:**
-   **Inaccurate Semantic Analysis**: The server's understanding is incomplete. It cannot know which code paths are "active" or "dead" for a specific build, nor can it compute the final value of any variable that depends on a build argument.
-   **Ambiguous Results**: LSP features may provide ambiguous results. For example, "Go to Definition" on a variable may navigate to multiple assignments across different conditional blocks.
-   **Diagnostic Mismatches**: The server's error checking may differ from `gn check`. It might produce false positives for code in an inactive block or miss errors in code that is currently disabled.

### Two-Tiered Analysis (Shallow vs. Full)

The analyzer uses a two-tiered strategy to balance performance, correctness, and feature richness.

-   **Shallow Analysis**: Performed on imported files (`.gni`). This pass analyzes the entire file, including statements inside conditionals, but crucially does not perform a deep analysis of `template` bodies. It identifies template *definitions* but not their internal logic. This is essential for two reasons:
    1.  **Performance**: It avoids costly, deep analysis of files that are only used for their top-level variables and template definitions.
    2.  **Correctness**: It prevents infinite recursion that could occur if `import` statements inside templates were analyzed without a specific invocation context.
-   **Full Analysis**: Performed on the primary file being edited (`.gn` or `.gni`). This is a deep, comprehensive pass that builds a complete semantic graph, resolving scopes, variable assignments, and dependencies. This detailed model powers most LSP features like "Go to Definition" and "Hover". The full analyzer leverages the cached results of the shallow analyzer for imported files to maintain performance.

### Caching and Performance

The server uses a freshness-checking mechanism to avoid re-analyzing unchanged files and their dependencies. The `CacheConfig` struct allows the server to differentiate between interactive requests (which might trigger a shallow update) and background requests.

### Concurrency

The server is built on `tokio` to handle multiple LSP requests concurrently without blocking. Shared state, such as the `DocumentStorage` and `Analyzer`, is managed safely across threads using `Arc<Mutex<T>>`.

### Background Indexing

For workspace-wide features like "Find All References," a complete view of the project is necessary. When a `.gn` file is first opened, a background task is spawned to walk the entire workspace directory, analyzing every `.gn` and `.gni` file. This populates the analyzer's cache. The indexer skips build output directories by checking for the presence of an `args.gn` file. Subsequent requests that need this global view can then wait for the indexing task to complete.

### Interaction with `gn` CLI

The server is designed to be mostly standalone but relies on the `gn` command-line tool for specific features where re-implementing the logic would be impractical.
-   **Location**: It has a built-in strategy to find the `gn` binary, looking in common prebuilt directories within a Chromium or Fuchsia checkout, or falling back to the system `PATH`.
-   **Formatting**: Document formatting is implemented by shelling out to `gn format --stdin`, leveraging the canonical formatter directly.

## 3. Core Components

The server is designed with a modular architecture, separating concerns into distinct components.

### Server (`server.rs`)

This is the main entry point of the application. It initializes the server, manages the LSP request/response lifecycle, and holds the shared state of the application, including the document storage and the analyzer.

### Document Storage (`storage.rs`)

This component acts as a cache for file contents. It distinguishes between:
1.  Files currently open and being edited in the client (in-memory).
2.  Files on disk that are part of the workspace but not open for editing.

It uses a combination of LSP document versions (for in-memory files) and file system modification timestamps (for on-disk files) to determine if a file is "fresh" or needs to be re-read.

### Parser (`ast/`)

The parser is responsible for turning raw text into a structured representation.
-   **Grammar (`gn.pest`)**: A formal grammar defines the syntax of the GN language. This makes the parser predictable and easy to maintain.
-   **AST (`ast/mod.rs`, `ast/parser.rs`)**: The raw parse tree from `pest` is transformed into a more ergonomic Abstract Syntax Tree (AST). The AST nodes provide methods for easy traversal and inspection, forming the input for the semantic analyzer.

### Semantic Analyzer (`analyze/`)

The analyzer is the brain of the language server. It consumes the AST and builds a rich semantic understanding of the code.

-   **Workspace Context**: The server establishes the workspace context by first finding the root directory, identified by a `.gn` file. This root path is essential for resolving source-absolute paths (e.g., `//path/to/file.cc`). The server then parses the `.gn` file to locate the main `buildconfig` file, which serves as the entry point for analyzing the build graph and understanding the default configuration.

-   **Key Data Structures**:
    -   `AnalyzedFile`: The complete semantic model for a single file.
    -   `AnalyzedEvent`: An enum representing a semantically significant occurrence in a file, such as an assignment, import, or target definition. The full analyzer's output is a stream of these events.
    -   `AnalyzedScope`: Represents a lexical scope as a tree, mapping variable names to their definitions and linking to parent scopes.
    -   `AnalyzedTarget`, `AnalyzedTemplate`: Represent defined targets and templates.
    -   `Link`: Represents a semantic connection from one file to another, such as a file path in a string or a target label in a `deps` list.

### LSP Feature Providers (`providers/`)

Each LSP feature is implemented in its own module. These providers consume the data from the Semantic Analyzer to generate responses for the client. Examples include `completion`, `hover`, `goto_definition`, and `references`.

## 4. Data Flow Example: "Go to Definition"

A typical request flows through the system as follows:

1.  **Request**: The user triggers "Go to Definition" on a variable in the editor. The client sends a `textDocument/definition` request to the server.
2.  **Dispatch**: `server.rs` receives the request and dispatches it to the `goto_definition` provider.
3.  **Analysis**: The provider requests the `AnalyzedFile` for the current document from the `Analyzer`.
4.  **Cache Check**: The `Analyzer` checks its cache for a fresh `AnalyzedFile`. If it's stale, it re-analyzes the file. This full analysis in turn uses the `ShallowAnalyzer` for any imported `.gni` files, which has its own cache of `ShallowAnalyzedFile` objects. This two-level caching ensures that only the necessary files are re-parsed.
5.  **Resolution**: The `goto_definition` provider inspects the `AnalyzedFile`. It finds the identifier under the cursor, looks up its definition within the current scope, and finds the location of the original assignment.
6.  **Response**: The provider constructs a `LocationLink` response containing the URI and position of the definition and returns it to the client, which then moves the user's cursor to the correct location.
