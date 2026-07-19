-- Windows-native becomes the default execution target. Before this
-- migration a NULL/blank `execution_target` meant "run inside WSL";
-- the dispatcher now treats anything other than 'wsl' as native, so
-- pin the previously-implicit rows to 'native' explicitly. Runners
-- deliberately set to 'wsl' keep running inside the distro.
UPDATE runners
   SET execution_target = 'native'
 WHERE execution_target IS NULL
    OR TRIM(execution_target) = '';
