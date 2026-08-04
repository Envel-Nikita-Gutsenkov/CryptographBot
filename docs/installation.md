# Cryptograph Installation & Deployment Guide

This guide describes how to build, run, and host the Cryptograph bot in both development and production environments.

## System Prerequisites
- **Operating System**: Linux (Fedora, Ubuntu, Debian), Windows (PowerShell), or macOS.
- **Rust Toolchain**: Rust 1.75+ (stable channel).
- **Docker** (Optional, recommended for production): Docker CE 20.10+ and docker-compose.

---

## 1. Local Configuration (`.env`)

Create a `.env` file in the root directory. You can copy the template from `.env.example`:

```ini
# --- Telegram API Config ---
TELEGRAM_BOT_TOKEN=123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11

# --- Allowed Admin User IDs (Comma-separated) ---
ALLOWED_ADMIN_IDS=111111111,222222222

# --- Data Sources Keys ---
CMC_API_KEY=your-coinmarketcap-pro-api-key
HELIUS_API_KEY=your-solana-helius-rpc-key

# --- Cryptographic Security ---
# Must be exactly 32 bytes represented in Hex (64 characters)
MASTER_KEY=767e0ca11656d80ba3124dca91f8c1c342230a1a99142ef31b1fbb6bb6525dfe

# --- System Settings ---
ALERT_INTERVAL_SECONDS=300
WALLET_POLL_MINUTES=15
BYN_RATE_URL=https://api.nbrb.by/exrates/rates/431
```

---

## 2. Running Locally (Development)

Build and run the project using cargo:

```powershell
# Copy config
copy .env.example .env

# Edit .env and supply tokens

# Build and run
cargo run
```

To run unit tests:
```powershell
cargo test
```

---

## 3. Production Deployment

### Option A: Using Docker (Recommended)
Docker ensures runtime isolation and sandboxing with no local build dependencies.

1. **Build the Docker Image**:
   ```bash
   docker build -t cryptograph:latest .
   ```

#### ARM64 / Remote Server Deployment (e.g., Ampere Altra)
If you are deploying to a remote ARM64 server (e.g., Ubuntu server), you can build the image natively on the server using the helper script:

1. **Package files on your local development machine**:
   Run the PowerShell script to collect all required files into the `dist` folder:
   ```powershell
   .\build_dist.ps1
   ```

2. **Deploy on the server**:
   - Copy the generated `dist` folder to your ARM64 server.
   - Configure the `.env` file on the server.
   - Execute the run script on the server:
     ```bash
     chmod +x run.sh
     ./run.sh
     ```

2. **Configure Docker Compose**:
   Create a `docker-compose.yml` file:
   ```yaml
   version: '3.8'
   services:
     bot:
       image: cryptograph:latest
       container_name: cryptograph_bot
       restart: unless-stopped
       env_file:
         - .env
       volumes:
         - ./data:/app/data
   ```

3. **Start the Service**:
   ```bash
   docker-compose up -d
   ```

### Option B: Systemd Service File (Linux Bare-Metal)
If you prefer running directly on a Linux host system:

1. Build the production binary:
   ```bash
   cargo build --release
   ```

2. Create a systemd service file `/etc/systemd/system/cryptograph.service`:
   ```ini
   [Unit]
   Description=Cryptograph Intelligence Telegram Bot
   After=network.target

   [Service]
   Type=simple
   User=cryptograph
   WorkingDirectory=/var/lib/cryptograph
   EnvironmentFile=/var/lib/cryptograph/.env
   ExecStart=/usr/local/bin/cryptograph
   Restart=always
   RestartSec=5

   [Install]
   WantedBy=multi-user.target
   ```

3. Enable and start:
   ```bash
   sudo systemctl daemon-reload
   sudo systemctl enable --now cryptograph
   ```

---

## 4. Updating the Application

To update the running bot on your remote server without encountering container conflicts or manual cleanup steps, follow these simple steps:

### Step 1: Package locally (Windows / Dev machine)
Run the packaging script in PowerShell to prepare the new release with the latest source code and git version hash:
```powershell
.\build_dist.ps1
```

### Step 2: Copy to Server
Copy the contents of the generated `dist` folder to the target directory on your remote Linux server (e.g., via SFTP, SCP, or Rsync).

### Step 3: Run the Update Command
On the server, run the helper script inside the target directory:
```bash
chmod +x run.sh
./run.sh
```

Or, execute Docker Compose directly:
```bash
docker compose up -d --build
```

*Note: Docker Compose automatically stops the old container, builds the new binary using Docker's build cache, and recreates the container seamlessly.*
