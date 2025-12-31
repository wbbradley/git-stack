#!/bin/bash
# Creates a synthetic repo with stacked branches for benchmarking git-stack
#
# Usage:
#   ./scripts/benchmark.sh [--json] [--iterations N]
#
# Options:
#   --json        Output results in JSON format
#   --iterations  Number of git-stack invocations to run (default: 10)

set -e

# Parse arguments
OUTPUT_FORMAT="human"
ITERATIONS=10

while [[ $# -gt 0 ]]; do
    case $1 in
        --json)
            OUTPUT_FORMAT="json"
            shift
            ;;
        --iterations)
            ITERATIONS="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Get the path to git-stack binary
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Build release version for accurate benchmarking
echo "Building git-stack in release mode..."
cargo build --release --manifest-path="$PROJECT_DIR/Cargo.toml" 2>/dev/null
GIT_STACK="$PROJECT_DIR/target/release/git-stack"

# Create temporary directory for benchmark
BENCH_DIR=$(mktemp -d)
echo "Created benchmark directory: $BENCH_DIR"

cleanup() {
    rm -rf "$BENCH_DIR"
}
trap cleanup EXIT

cd "$BENCH_DIR"

# Initialize git repo
git init --quiet
git config user.email "benchmark@test.com"
git config user.name "Benchmark"

# Create initial commit
git commit --allow-empty -m "initial" --quiet

# Create a bare remote repo and add it as origin
REMOTE_DIR="$BENCH_DIR/remote.git"
git init --bare --quiet "$REMOTE_DIR"
git remote add origin "$REMOTE_DIR"
git push --quiet -u origin main

# Set up origin/HEAD to point to main (normally done by clone)
git remote set-head origin main

# Create main branch with several commits
echo "Creating main branch with commits..."
for i in {1..5}; do
    echo "Main content $i - $(date +%s%N)" > "file$i.txt"
    git add .
    git commit -m "main commit $i" --quiet
done

# Push main to remote so git-stack can find origin/main
git push --quiet origin main

# Create first stack: feature-a -> feature-a-1 -> feature-a-2
echo "Creating feature-a stack..."
git checkout -b feature-a --quiet
echo "Feature A implementation" > feature-a.txt
echo "Additional feature A code" >> feature-a.txt
git add .
git commit -m "implement feature a" --quiet

git checkout -b feature-a-1 --quiet
echo "Feature A-1 sub-feature" > feature-a-1.txt
git add .
git commit -m "implement feature a-1" --quiet

git checkout -b feature-a-2 --quiet
echo "Feature A-2 sub-feature" > feature-a-2.txt
git add .
git commit -m "implement feature a-2" --quiet

# Create second stack: feature-b -> feature-b-1
echo "Creating feature-b stack..."
git checkout main --quiet
git checkout -b feature-b --quiet
echo "Feature B implementation" > feature-b.txt
git add .
git commit -m "implement feature b" --quiet

git checkout -b feature-b-1 --quiet
echo "Feature B-1 sub-feature" > feature-b-1.txt
git add .
git commit -m "implement feature b-1" --quiet

# Create third stack (deeper): feature-c -> feature-c-1 -> feature-c-2 -> feature-c-3
echo "Creating feature-c stack (deeper)..."
git checkout main --quiet
git checkout -b feature-c --quiet
echo "Feature C implementation" > feature-c.txt
git add .
git commit -m "implement feature c" --quiet

git checkout -b feature-c-1 --quiet
echo "Feature C-1" > feature-c-1.txt
git add .
git commit -m "implement feature c-1" --quiet

git checkout -b feature-c-2 --quiet
echo "Feature C-2" > feature-c-2.txt
git add .
git commit -m "implement feature c-2" --quiet

git checkout -b feature-c-3 --quiet
echo "Feature C-3" > feature-c-3.txt
git add .
git commit -m "implement feature c-3" --quiet

# Initialize git-stack tracking for all branches
echo "Setting up git-stack tracking..."
git checkout feature-a --quiet
"$GIT_STACK" mount main 2>/dev/null || true

git checkout feature-a-1 --quiet
"$GIT_STACK" mount feature-a 2>/dev/null || true

git checkout feature-a-2 --quiet
"$GIT_STACK" mount feature-a-1 2>/dev/null || true

git checkout feature-b --quiet
"$GIT_STACK" mount main 2>/dev/null || true

git checkout feature-b-1 --quiet
"$GIT_STACK" mount feature-b 2>/dev/null || true

git checkout feature-c --quiet
"$GIT_STACK" mount main 2>/dev/null || true

git checkout feature-c-1 --quiet
"$GIT_STACK" mount feature-c 2>/dev/null || true

git checkout feature-c-2 --quiet
"$GIT_STACK" mount feature-c-1 2>/dev/null || true

git checkout feature-c-3 --quiet
"$GIT_STACK" mount feature-c-2 2>/dev/null || true

# Go to a branch in the middle of a stack for realistic benchmark
git checkout feature-a-1 --quiet

echo ""
echo "=== Benchmark Setup Complete ==="
echo "Branches: main, feature-a, feature-a-1, feature-a-2, feature-b, feature-b-1, feature-c, feature-c-1, feature-c-2, feature-c-3"
echo "Current branch: feature-a-1"
echo ""
echo "=== Running Benchmark: $ITERATIONS invocations of 'git stack' ==="
echo ""

# Run benchmark
if [ "$OUTPUT_FORMAT" = "json" ]; then
    # JSON output mode - collect all results
    echo "["
    for i in $(seq 1 $ITERATIONS); do
        if [ $i -gt 1 ]; then
            echo ","
        fi
        GIT_STACK_BENCHMARK=1 GIT_STACK_BENCHMARK_JSON=1 "$GIT_STACK" 2>&1 | tail -n +2
    done
    echo "]"
else
    # Human-readable output mode
    RESULTS_FILE="$BENCH_DIR/results.txt"

    for i in $(seq 1 $ITERATIONS); do
        echo "Run $i/$ITERATIONS..."
        GIT_STACK_BENCHMARK=1 "$GIT_STACK" 2>&1 | tee -a "$RESULTS_FILE"
        echo "---" >> "$RESULTS_FILE"
    done

    echo ""
    echo "=== Benchmark Complete ==="
    echo "Raw results saved to: $RESULTS_FILE"

    # Calculate summary statistics from the last run
    echo ""
    echo "Last run summary shown above."
fi
