use crate::model::ComputerSet;

pub(crate) fn normalize(set: &mut ComputerSet) {
    let s = &mut set.spec;
    s.volumes.sort_by(|a, b| a.name.cmp(&b.name));
    s.workspaces.sort_by(|a, b| a.name.cmp(&b.name));
    s.sandboxes.sort_by(|a, b| a.name.cmp(&b.name));
    s.apps.sort_by(|a, b| a.name.cmp(&b.name));
    s.agents.sort_by(|a, b| a.name.cmp(&b.name));
    s.computers.sort_by(|a, b| a.name.cmp(&b.name));
    for sandbox in &mut s.sandboxes {
        sandbox.mounts.sort_by(|a, b| a.path.cmp(&b.path));
    }
    for app in &mut s.apps {
        app.state_paths.sort();
        app.export_paths.sort();
        // argv is intentionally ordered; sorting it would change program semantics.
    }
    for agent in &mut s.agents {
        agent.capabilities.sort();
        agent.secret_refs.sort();
    }
    for computer in &mut s.computers {
        computer.sandbox_refs.sort();
        computer.app_refs.sort();
    }
}
