# Grafana Profit Dashboard Quickstart (No-Experience Edition)

This walkthrough assumes you have never managed servers before. Follow every step in order. Do not skip anything. When in doubt, re-read the step and repeat it slowly. The goal: see Arbot’s profits in Grafana so you know the bot is worth running.

---

## Before You Start

1. **Computer:** Use a Linux machine (Ubuntu 22.04 or similar). If you use macOS, the commands still work. Windows users should run the commands inside [Windows Subsystem for Linux (WSL)](https://learn.microsoft.com/windows/wsl/install).
2. **Arbot Folder:** Confirm you already downloaded this repository and that the folder `arbot` is on your computer.
3. **Terminal Window:** Open a terminal (also called “command line”). On Linux press <kbd>Ctrl</kbd> + <kbd>Alt</kbd> + <kbd>T</kbd>. On macOS open **Terminal** from Spotlight. On Windows (WSL) open **Ubuntu** from the Start menu.
4. **Stay in One Window:** Keep this terminal open the entire time. Every command below is typed (or pasted) into this window and confirmed with the <kbd>Enter</kbd> key.

---

## Step 1 – Start Arbot With Metrics Enabled

1. In the terminal, change into the Arbot folder:
   
   ```bash
   cd /path/to/arbot
   ```
   
   Replace `/path/to/arbot` with the actual folder location if it is different.

2. Tell Arbot which port to use for metrics and then launch it:
   
   ```bash
   export PROMETHEUS_PORT=9000
   RUST_LOG=info cargo run --release
   ```
   
   - After you press <kbd>Enter</kbd>, Arbot starts and prints scrolling text. Leave this terminal running. It keeps exposing live profit metrics at `http://localhost:9000/metrics`.
   - If you ever close this window, Arbot stops. Do **not** close it while following this guide.

Leave the Arbot terminal running and open a **second** terminal window for the remaining steps.

---

## Step 2 – Create the Prometheus Settings File

1. In the second terminal, make sure you are in the same Arbot folder:
   
   ```bash
   cd /path/to/arbot
   ```

2. Copy and paste the block below exactly. Press <kbd>Enter</kbd> after the last `EOF` line. This creates a file named `prometheus.yml`. Do **not** edit the text.
   
   ```bash
   cat <<'EOF' > prometheus.yml
   global:
     scrape_interval: 5s
   scrape_configs:
     - job_name: "arbot"
       static_configs:
         - targets: ["localhost:9000"]
   EOF
   ```

3. Confirm the file exists:
   
   ```bash
   ls prometheus.yml
   ```
   
   The terminal must print `prometheus.yml`. If it prints “No such file or directory,” repeat Step 2.

---

## Step 3 – Install Prometheus (Data Collector)

1. Still in the second terminal, download Prometheus:
   
   ```bash
   curl -LO https://github.com/prometheus/prometheus/releases/download/v2.52.0/prometheus-2.52.0.linux-amd64.tar.gz
   ```
   
   Wait until the command finishes; you will return to a prompt that ends with `$`.

2. Unpack the download:
   
   ```bash
   tar -xf prometheus-2.52.0.linux-amd64.tar.gz
   ```

3. Move into the new folder:
   
   ```bash
   cd prometheus-2.52.0.linux-amd64
   ```

4. Copy the settings file into the Prometheus folder:
   
   ```bash
   cp ../prometheus.yml .
   ```

5. Create a place to store Prometheus data:
   
   ```bash
   mkdir -p data
   ```

6. Start Prometheus now:
   
   ```bash
   ./prometheus --config.file="$(pwd)/prometheus.yml" --storage.tsdb.path="$(pwd)/data"
   ```
   
   - Prometheus prints lines that include `Server is ready to receive web requests.` Leave this terminal open; Prometheus must keep running.
   - Prometheus listens on `http://localhost:9090`. Do **not** stop it.

You now have two terminals running: one for Arbot, one for Prometheus. Open a **third** terminal to continue.

---

## Step 4 – Install Grafana (Dashboard)

1. In the third terminal, go back to the Arbot folder:
   
   ```bash
   cd /path/to/arbot
   ```

2. Download Grafana:
   
   ```bash
   curl -LO https://dl.grafana.com/oss/release/grafana-10.4.2.linux-amd64.tar.gz
   ```

3. Unpack the file:
   
   ```bash
   tar -xf grafana-10.4.2.linux-amd64.tar.gz
   ```

4. Enter the Grafana folder:
   
   ```bash
   cd grafana-v10.4.2
   ```

5. Start Grafana:
   
   ```bash
   ./bin/grafana-server web
   ```
   
   - The terminal shows log lines. Wait for `HTTP Server Listen` to appear. Leave this terminal window open; Grafana must keep running.
   - Grafana uses `http://localhost:3000` and default login `admin` / `admin`. You will change the password soon.

You now have **three** terminals running services. Do not close any of them until the guide says you can.

---

## Step 5 – Connect Grafana to Prometheus

1. Open a web browser on the same computer (Firefox, Chrome, etc.).
2. In the address bar, type `http://localhost:3000` and press <kbd>Enter</kbd>.
3. Log in with username `admin` and password `admin`.
4. Grafana asks for a new password. Type a strong password you will remember and click **Save**.
5. On the left menu, click the gear icon ⚙️ (labeled **Connections**) and then click **Data sources**.
6. Click the blue **Add data source** button.
7. Choose **Prometheus** from the list.
8. In the **HTTP** section, set **URL** to `http://localhost:9090`.
9. Scroll to the bottom and click **Save & test**. You must see the green message “Data source is working.” If you see an error, double-check that Prometheus is still running in its terminal window.

---

## Step 6 – Load the Arbot Dashboard

1. In Grafana, click the four-square icon (left menu) and select **Dashboards**.
2. Click the **Import** button.
3. Click **Upload JSON file**.
4. In the file picker, navigate to your Arbot folder, then `docs/grafana`, and choose `arbot-dashboard.json`. Click **Open**.
5. Grafana asks which data source to use. Pick the Prometheus data source you created in Step 5.
6. Click **Import**. The dashboard loads immediately.
7. Watch the panels fill with data over the next minute. Key panels show:
   - **Net Profit (24h):** cumulative net PnL trend for the last day.
   - **Execution Win Rate:** share of detected opportunities that execute successfully.
   - **Gas Efficiency:** ratio of profit velocity to gas spend velocity.
   - **Opportunity Funnel:** detected vs executed vs failed opportunities.

### Also import the Profit Funnel dashboard

Repeat the import steps but choose **`arbot-funnel.json`** ("Arbot — Profit Funnel").
This is the diagnostic view for the $833/day ($25k/mo) target: it shows the full
funnel (Detected → Seen → Sim-passed → Broadcast → Included → Executed), the
conversion rates between stages (sim-pass %, inclusion %, revert %, relay-reject %),
a **Projected $/day** stat against the $833 line, loss/failure signals (RPC errors,
reverts, relay rejects, search timeouts), and per-stage latency. The panel header
explains how to read the funnel to localise the bottleneck (edge-limited vs
RPC/latency-limited vs capital-limited).

New metric backing the funnel: `tx_relay_rejected_total{chain,strategy}` counts
private-relay rejections (a distinct loss stage). Not yet instrumented:
realised-vs-simulated net, which needs on-chain profit extraction from the executor
receipt — until then the funnel measures inclusion, not slippage/L1-fee truth.

- **Circuit Control Mode:** reminder that breaker controls are manual-only (`tripCircuit` / `resetCircuit`).

If the panels stay empty, re-check Steps 1–5. Most issues come from Prometheus or Arbot not running.

---

## Step 7 – Keep Everything Running

1. Leave all three terminal windows open while you monitor profits. Closing any window stops its service.
2. When you are done for the day, stop each program safely:
   - In the Grafana terminal, press <kbd>Ctrl</kbd> + <kbd>C</kbd> once.
   - Switch to the Prometheus terminal and press <kbd>Ctrl</kbd> + <kbd>C</kbd> once.
   - Switch to the Arbot terminal and press <kbd>Ctrl</kbd> + <kbd>C</kbd> once.
3. Next time you want the dashboard, repeat Steps 1 through 6. After a few runs you can automate these steps with system services, but first prove the stack works manually.

---

## Optional Hardening (After You Are Comfortable)

- Convert Prometheus and Grafana into system services (systemd or supervisord) so they restart automatically after crashes.
- Restrict access to Grafana with a VPN or firewall; competitors monitoring your stats erase your advantage.
- Configure Grafana alerts for negative profit, slow latency, or relay failures so you can react before real money disappears.

Remember: this dashboard exists for a single reason—verify that Arbot is generating more profit than it spends. If a step stops working, fix it immediately before running the bot blindly.
