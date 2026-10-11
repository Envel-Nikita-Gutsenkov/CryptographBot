# Compile Cryptograph inside Docker on the target machine
Write-Host "Preparing distribution files..."

# Create dist directory
$distDir = ".\dist"
if (Test-Path $distDir) {
    Remove-Item -Recurse -Force $distDir
}
New-Item -ItemType Directory -Path $distDir | Out-Null

# Copy assets
Write-Host "Copying deployment assets..."
# NOTE: .env is intentionally NOT copied — the server already has the correct
# one and uploading the template would overwrite real secrets (tokens/keys).
Copy-Item ".\docker-compose.yml" -Destination "$distDir\docker-compose.yml"
Copy-Item ".\Dockerfile" -Destination "$distDir\Dockerfile"
Copy-Item ".\Cargo.toml" -Destination "$distDir\Cargo.toml"
Copy-Item ".\Cargo.lock" -Destination "$distDir\Cargo.lock"
Copy-Item ".\models.json" -Destination "$distDir\models.json"
Copy-Item -Recurse ".\src" -Destination "$distDir\src"

# Generate dynamic build hash (Git commit hash or timestamp fallback)
$buildHash = (git rev-parse --short HEAD 2>$null)
if (-not $buildHash) {
    $buildHash = Get-Date -Format "yyyyMMdd-HHmm"
}
$buildHash = $buildHash.Trim()
[System.IO.File]::WriteAllText("$distDir\src\bot\version.txt", $buildHash)
Write-Host "Generated Build Hash: $buildHash"

# Create a build-and-run wrapper script for the Linux ARM64 server
$runScriptContent = @'
#!/usr/bin/env bash
# This script will build the Docker container locally on your Linux ARM64 server
# and launch the bot. This avoids high memory spikes during compilation on the server
# by leveraging Docker's caching layers.

# 1. Clean up obsolete models.rs file if it exists on the host
if [ -f "src/models.rs" ]; then
  echo "🧹 Removing obsolete src/models.rs to prevent module ambiguity..."
  rm "src/models.rs"
fi

echo "🐳 Building Docker image for ARM64..."
if docker compose version >/dev/null 2>&1; then
  docker compose build
else
  docker-compose build
fi

echo "🛑 Stopping and removing existing bot containers..."
# Stop both compose-managed container and raw docker-managed container
if docker compose version >/dev/null 2>&1; then
  docker compose down 2>/dev/null || true
else
  docker-compose down 2>/dev/null || true
fi
docker stop cryptograph_bot 2>/dev/null || true
docker rm cryptograph_bot 2>/dev/null || true
docker stop cryptograph 2>/dev/null || true
docker rm cryptograph 2>/dev/null || true

echo "🚀 Launching bot in background..."
if docker compose version >/dev/null 2>&1; then
  docker compose up -d
  echo "✅ Running! Use 'docker compose logs -f' to see logs."
else
  docker-compose up -d
  echo "✅ Running! Use 'docker-compose logs -f' to see logs."
fi

# 🔍 Post-deploy sanity check: make sure SQLite lives in the persistent volume,
# otherwise portfolios/stats/DeepSeek budget reset on every container rebuild.
sleep 3
echo "🔍 Verifying SQLite persistence..."
docker compose logs cryptograph 2>/dev/null | grep -E "SQLite database path|SQLite database initialized" | tail -2
docker compose exec -T cryptograph sh -c 'test -f /app/data/crypto.db && echo "✅ DB is in the persistent volume (/app/data/crypto.db)" || echo "❌ DB NOT found in volume — check SQLITE_DB=/app/data/crypto.db in docker-compose.yml"'
'@

[System.IO.File]::WriteAllText("$distDir\run.sh", $runScriptContent.Replace("`r`n", "`n"), (New-Object System.Text.UTF8Encoding($false)))

Write-Host "Done! Everything you need to transfer to your ARM64 server is inside the 'dist' directory."
Write-Host "Copy 'dist' contents to your server, fill in .env, and run './run.sh'."
Get-ChildItem $distDir | Select-Object Name
