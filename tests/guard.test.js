import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import {readGuard, writeGuardCountry} from '../psiphon-tunnel@terentiy/guard.js';
import {assert, equal, test} from './harness.js';

test('the Claude picker updates its own policy, and rejects stale country status', () => {
    const dir = GLib.dir_make_tmp('pt-guard-XXXXXX');
    const conf = `${dir}/client.conf`;
    const policy = `${dir}/claude-country.json`;
    const status = `${dir}/status.json`;
    try {
        GLib.file_set_contents(conf, '{"EgressRegion":"AT"}\n');
        equal(readGuard(dir, status), null);
        GLib.file_set_contents(policy, '{"service":"claude","country":"AT"}');
        GLib.file_set_contents(status, '{"service":"claude","country":"AT","phase":"ready","pid":42}');
        equal(readGuard(dir, status).phase, 'ready');
        equal(readGuard(dir, status).pid, 42);
        writeGuardCountry(dir, 'DE');
        equal(readGuard(dir, status).country, 'DE');
        equal(readGuard(dir, status).phase, 'unavailable');
        const [, bytes] = GLib.file_get_contents(conf);
        equal(JSON.parse(new TextDecoder().decode(bytes)).EgressRegion, 'AT');
        let rejected = false;
        try {
            writeGuardCountry(dir, '');
        } catch {
            rejected = true;
        }
        assert(rejected, 'the fastest-region option must not become an access policy');
        equal(readGuard(dir, status).country, 'DE');
    } finally {
        for (const name of [conf, policy, status, dir])
            if (GLib.file_test(name, GLib.FileTest.EXISTS))
                Gio.File.new_for_path(name).delete(null);
    }
});
