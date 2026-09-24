<?php
// The one part of Mage-OS's generator the compile reads: the baseline snapshot
// guard from mage-os/mageos-magento2#299, which also fires on the primary pass.

namespace Magento\Framework\Interception;

class PluginListGenerator
{
    public function write(array $scopes): void
    {
        foreach ($scopes as $scope) {
            if ($scope === 'global' || $scope === 'primary') {
                $this->globalScopePluginData = $this->pluginData;
            }
        }
    }
}
