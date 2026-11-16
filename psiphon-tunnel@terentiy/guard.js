import GLib from 'gi://GLib';

const POLICY = 'claude-country.json';
const STATUS = '/run/claude-at/status.json';
const PHASES = new Set(['ready', 'checking', 'blocked', 'failed', 'unavailable']);
const validCountry = code => typeof code === 'string' && /^[A-Z]{2}$/.test(code);
const policyPath = dir => GLib.build_filenamev([dir, POLICY]);

function readJson(path) {
    try {
        const [, contents] = GLib.file_get_contents(path);
        return JSON.parse(new TextDecoder().decode(contents));
    } catch {
        return null;
    }
}

export function readGuard(dir, statusPath = STATUS) {
    const policy = readJson(policyPath(dir));
    if (policy?.service !== 'claude' || !validCountry(policy.country))
        return null;
    const status = readJson(statusPath);
    const phase = status?.service === 'claude' && status.country === policy.country && PHASES.has(status.phase)
        ? status.phase : 'unavailable';
    const pid = Number.isInteger(status?.pid) && status.pid > 1 ? status.pid : null;
    return {country: policy.country, phase, pid};
}

export function writeGuardCountry(dir, country) {
    if (!validCountry(country))
        throw new Error('Expected a two-letter country code');
    if (!readGuard(dir))
        throw new Error('Claude country protection is not installed');
    GLib.file_set_contents_full(policyPath(dir), JSON.stringify({service: 'claude', country}) + '\n',
        GLib.FileSetContentsFlags.CONSISTENT, 0o600);
}
