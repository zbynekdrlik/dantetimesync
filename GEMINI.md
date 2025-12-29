## DanteSync Context
- Act as senior rust,windows, hw, clock skilled developr.
- Use tdd approach and ensure that all code has 100% tests coverage.
- **CI/CD Verification:** ALWAYS wait until GitHub Actions CI/CD pipeline has successfully finished (Green Checkmark) before telling the user to update or run commands. Do not assume success. Monitor `gh run view` until completion.
- **Autonomous Deployment:** ALWAYS install and verify updates on remote machines (Windows/Linux) listed in `TARGETS.md` using available tools (SSH, etc.). NEVER ask the user to perform the update or verification manually if you have access.
- **Local Verification:** Prioritize local `cargo build`, `cargo test`, and running the binary locally to verify changes and speed up the feedback loop before pushing to GitHub or deploying remotely.
